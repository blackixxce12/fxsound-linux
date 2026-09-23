//! The controller: the one place that turns what the user did into real effects.
//!
//! This is the port of `FxController` (`fxsound/Source/GUI/FxController.cpp`, 2914 lines), minus
//! its broadcaster. The original is a singleton that every view reaches into; here the data flows
//! one way:
//!
//! ```text
//!   UiState ──► views render ──► UiResponse(Vec<UiAction>) ──► App::handle ──┐
//!      ▲                                                                      │
//!      └──────────── meters, device list, preset list ◄────────── engine ◄────┘
//! ```
//!
//! Nothing below the views knows about egui, and nothing in the UI crate knows about PipeWire or
//! the file system, so each half can be tested without the other.

use fxsound_audio::EngineHandle;
use fxsound_core::{
    AudioDevice, DeviceDirection, Effect, EqBand, Preset, Settings, ThemeMode, ViewMode,
    messages::{AudioToUi, DspEvent, DspParams, InputDspParams, Meters, UiToAudio},
    scale,
};
use fxsound_preset::{InputPresetStore, PresetFile, PresetStore, Store, input::InputPreset};
use fxsound_ui::{
    AssetCache, Palette, UiAction, UiState,
    dialogs::{
        CalibrationAction, CalibrationView, ExportState, ImportState, ImportSummary,
        OverwriteChoice, PresetsAction,
    },
    state::PresetEntry,
};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::audio_link::{AudioLink, FakeEngine};
use crate::calibration::{
    self, Calibration, CalibrationState, Command as CalibrationCommand, Lane,
};
use crate::cli::PresetCommand;
use crate::events::{AppEvent, LaneState, Published};
use crate::notify::{Message, Notifier};
use crate::priority;
use fxsound_core::i18n::{self, tr, tr_args};
use fxsound_core::settings::CalibrationRecord;
use fxsound_ui::dialogs::settings::{DevicePriority, SettingsState};

/// The characters `PresetNameInputFilter` strips from a typed preset name
/// (`FxPresetNameEditor.cpp:6-33`): the Windows reserved-filename set, kept on Linux so a preset
/// saved here can be copied to a Windows FxSound unchanged (`docs/spec/03-controls.md` §11.2).
pub const FORBIDDEN_PRESET_NAME_CHARS: &str = "<>:\"/\\|?*";

/// `setInputRestrictions(64)` (`FxPresetNameEditor.cpp:52`, `FxMainWindow.cpp:58`).
pub const MAX_PRESET_NAME_CHARS: usize = 64;

/// How long after the engine first reports a new per-device volume ([`AudioToUi::TargetVolume`])
/// the settings file is written. Counted from the first report, not the last, so a volume that
/// keeps moving is still saved about once a second, and a mixer drag costs one write rather than
/// one per step. Whatever is still unsaved at exit is written by [`App::shutdown`].
const VOLUME_SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// `FxModel::isPresetNameValid` (`FxModel.cpp:142-153`): a name can be used for a new or renamed
/// preset when it is not blank and no preset already has it, compared case-insensitively.
#[must_use]
pub fn preset_name_available(existing: &[PresetEntry], name: &str) -> bool {
    let wanted = name.trim();
    if wanted.is_empty() {
        return false;
    }
    let wanted = wanted.to_lowercase();
    !existing.iter().any(|p| p.name.to_lowercase() == wanted)
}

/// Why a preset command is refused: the reasons the hamburger menu greys an item out
/// (`FxMainWindow.cpp:536-543`), which the command line, the control socket and D-Bus answer with
/// rather than doing what the menu would never offer (upstream `FxController.cpp:377-448`). One
/// rule, [`App::preset_command_allowed`], for all of them.
///
/// The text ([`std::fmt::Display`]) is what `fxsound` prints on stderr and what a D-Bus caller
/// gets with `org.fxsound.FxSound.Error.Refused`: English, and naming the options that get round
/// it, like every other refusal of the command path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// `--preset` with a name the edit direction's list does not have; whether the other lane's
    /// list has it, which is what the message then says how to reach.
    UnknownPreset {
        lane: DeviceDirection,
        name: String,
        other_lane_has_it: bool,
    },
    /// Nothing is selected to save, overwrite, undo, rename or delete.
    NoPresetSelected,
    /// Save New Preset or Overwrite with no unsaved changes: there is nothing to save.
    NothingToSave {
        preset: String,
    },
    /// Undo with no unsaved changes.
    NothingToUndo {
        preset: String,
    },
    /// Overwrite of a preset that ships with FxSound, which would leave a user copy shadowing it.
    FactoryOverwrite {
        preset: String,
    },
    FactoryRename {
        preset: String,
    },
    FactoryDelete {
        preset: String,
    },
    /// Rename of a preset with unsaved changes: what moves is the saved file, and the edits would
    /// be left behind under a name that is gone.
    UnsavedChanges {
        preset: String,
    },
    /// Save New Preset or Rename to a name a preset already has, compared case-insensitively.
    NameTaken {
        name: String,
    },
    /// Save New Preset or Rename to a blank name.
    EmptyName,
    /// Save New Preset at the user-preset cap ([`App::max_user_presets`]).
    LimitReached {
        max: usize,
    },
}

impl Refusal {
    /// Whether the name is all that is wrong: Save New Preset and Rename Preset open an editor
    /// under their row that asks for the name and checks it as it is typed, so the row itself is
    /// offered whatever the name.
    #[must_use]
    pub const fn is_about_the_name(&self) -> bool {
        matches!(self, Self::NameTaken { .. } | Self::EmptyName)
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPreset {
                lane,
                name,
                other_lane_has_it,
            } => {
                write!(f, "no {} preset is called {name:?}", lane.key())?;
                if *other_lane_has_it {
                    let other = lane.other();
                    write!(
                        f,
                        "; it is {} preset, so add --edit={} to select it",
                        lane_noun(other),
                        other.key()
                    )?;
                }
                Ok(())
            }
            Self::NoPresetSelected => f.write_str("no preset is selected"),
            Self::NothingToSave { preset } => {
                write!(f, "{preset:?} has no unsaved changes to save")
            }
            Self::NothingToUndo { preset } => {
                write!(f, "{preset:?} has no unsaved changes to undo")
            }
            Self::FactoryOverwrite { preset } => write!(
                f,
                "{preset:?} is a factory preset and cannot be overwritten; save the changes as a \
                 new preset with --save_preset"
            ),
            Self::FactoryRename { preset } => {
                write!(f, "{preset:?} is a factory preset and cannot be renamed")
            }
            Self::FactoryDelete { preset } => {
                write!(f, "{preset:?} is a factory preset and cannot be deleted")
            }
            Self::UnsavedChanges { preset } => write!(
                f,
                "{preset:?} has unsaved changes; save them with --overwrite_preset or drop them \
                 with --undo_preset before renaming it"
            ),
            Self::NameTaken { name } => write!(f, "a preset called {name:?} already exists"),
            Self::EmptyName => f.write_str("a preset name cannot be empty"),
            Self::LimitReached { max } => write!(
                f,
                "the limit of {max} user presets is reached; delete one before saving another"
            ),
        }
    }
}

/// "an output" / "an input", for a message about a lane's preset.
const fn lane_noun(lane: DeviceDirection) -> &'static str {
    match lane {
        DeviceDirection::Output => "an output",
        DeviceDirection::Input => "an input",
    }
}

/// Which of the hamburger menu's preset items are offered: each is
/// [`App::preset_command_allowed`] for its command, while the power is on
/// (`FxMainWindow.cpp:536-543`). Export and Import are not preset commands and stay the menu's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PresetMenu {
    pub save_new: bool,
    pub overwrite: bool,
    pub undo: bool,
    pub rename: bool,
    pub delete: bool,
}

/// What the window shows for one lane: the design's `ChainControls` (0.4.0 design §1.4), of which
/// [`App`] keeps one per lane ([`App::lane_controls`]).
///
/// A lane's controls live in [`UiState`] while that lane is the edit direction, because that is
/// what the views read and write. Switching the edit direction stores them in the lane's slot and
/// puts the other lane's back, so a look at the speakers' list never costs an unsaved equalizer
/// move on the microphone, and never re-applies a preset under a chain that is running. The
/// published snapshots (`params`, `input_params`) are what the engine runs; this is only what the
/// window shows.
#[derive(Debug, Clone)]
struct LaneControls {
    presets: Vec<PresetEntry>,
    selected_preset: Option<usize>,
    effects: [f32; Effect::COUNT],
    eq_on: bool,
    eq_bands: Vec<EqBand>,
    filter_q: f32,
    master_gain_db: f32,
    balance_db: f32,
    volume_leveling: f32,
    gate_on: bool,
    compressor_on: bool,
    deesser_on: bool,
    denoise_on: bool,
}

impl LaneControls {
    fn take(state: &UiState) -> Self {
        Self {
            presets: state.presets.clone(),
            selected_preset: state.selected_preset,
            effects: state.effects,
            eq_on: state.eq_on,
            eq_bands: state.eq_bands.clone(),
            filter_q: state.filter_q,
            master_gain_db: state.master_gain_db,
            balance_db: state.balance_db,
            volume_leveling: state.volume_leveling,
            gate_on: state.gate_on,
            compressor_on: state.compressor_on,
            deesser_on: state.deesser_on,
            denoise_on: state.denoise_on,
        }
    }

    fn put(self, state: &mut UiState) {
        state.presets = self.presets;
        state.selected_preset = self.selected_preset;
        state.effects = self.effects;
        state.eq_on = self.eq_on;
        state.eq_bands = self.eq_bands;
        state.filter_q = self.filter_q;
        state.master_gain_db = self.master_gain_db;
        state.balance_db = self.balance_db;
        state.volume_leveling = self.volume_leveling;
        state.gate_on = self.gate_on;
        state.compressor_on = self.compressor_on;
        state.deesser_on = self.deesser_on;
        state.denoise_on = self.denoise_on;
    }
}

/// What the selected voice preset itself says about the stages the Settings pane can override.
///
/// Captured when a preset is applied, because the published snapshot holds the *effective* values
/// — the override written over the preset — and an override set back to `Preset` has to find what
/// the preset said underneath it (0.4.0 design §2 & 3: the settings win when not `Preset`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PresetVoicing {
    /// The master switch, which a preset with no `[denoise]` table and `rnnoise = false` leaves
    /// off whatever its level says.
    pub(crate) rnnoise: bool,
    pub(crate) denoise_level: fxsound_core::DenoiseLevel,
    pub(crate) denoise_channels: fxsound_core::DenoiseChannelMode,
    /// The preset's own row, which may differ from its level's: kept whenever the level in force
    /// is the preset's.
    pub(crate) denoise_control: fxsound_core::DenoiseControl,
    pub(crate) deesser_mode: fxsound_core::DeEsserMode,
    pub(crate) dereverb: fxsound_core::DereverbLevel,
}

impl PresetVoicing {
    fn of(params: &InputDspParams) -> Self {
        Self {
            rnnoise: params.rnnoise,
            denoise_level: params.denoise_level,
            denoise_channels: params.denoise_channels,
            denoise_control: params.denoise_control,
            deesser_mode: params.deesser_mode,
            dereverb: params.dereverb,
        }
    }
}

/// Write the Settings pane's microphone settings over what the voice preset said.
///
/// - **Noise suppression** and **denoiser channels** are overrides: `Preset` follows the preset,
///   anything else wins. A pinned level also decides the master switch — `Off` stops the stage,
///   any other level runs it even on a preset that has no denoiser — and brings its own table
///   row, unless it is the level the preset already names, whose tuned row is kept.
/// - **De-esser mode** and **de-reverb** have no `Preset` choice, so they are read as "at least":
///   `Classic` and `Off`, the defaults, leave the preset's choice alone, and `Adaptive` or a
///   de-reverb level ask for more than a preset that says less. A shipped preset that asks for the
///   adaptive de-esser (Gaming Headset) keeps it on a fresh install; the design's "off unless a
///   preset or the setting asks" (§7) is the same rule for the de-reverb.
pub(crate) fn apply_microphone_settings(
    params: &mut InputDspParams,
    preset: &PresetVoicing,
    settings: &Settings,
) {
    use fxsound_core::{DeEsserMode, DenoiseLevel, DereverbLevel};

    let level = settings.noise_suppression.resolve(preset.denoise_level);
    params.denoise_level = level;
    params.denoise_control = if level == preset.denoise_level {
        preset.denoise_control
    } else {
        level.control()
    };
    params.rnnoise = match settings.noise_suppression.level() {
        Some(pinned) => pinned != DenoiseLevel::Off,
        None => preset.rnnoise,
    };
    params.denoise_channels = settings.denoise_channels.resolve(preset.denoise_channels);

    params.deesser_mode = if settings.deesser_mode == DeEsserMode::Adaptive {
        DeEsserMode::Adaptive
    } else {
        preset.deesser_mode
    };
    let rank = |level: DereverbLevel| DereverbLevel::ALL.iter().position(|l| *l == level);
    params.dereverb = if rank(settings.dereverb) > rank(preset.dereverb) {
        settings.dereverb
    } else {
        preset.dereverb
    };
}

/// A lane's controls as the kind of preset file that lane keeps: a `.fac` for the speakers, a TOML
/// voice preset for the microphone.
///
/// Every path that writes a preset — Save, Save New Preset, the autosave on switching away and the
/// one on the way out — builds one of these from the lane it is about and hands it to that lane's
/// store ([`App::autosave_lane_preset`], [`App::save_lane_preset`]). 0.3.0 built a `.fac` from the
/// window whichever lane it showed, so saving on a microphone filed the voice preset's equalizer
/// as a music preset named after it, in the speakers' store; with the kind carried in the type,
/// a voice preset has no route into the `.fac` store at all.
#[derive(Debug, Clone, PartialEq)]
enum LanePreset {
    Music(Preset),
    Voice(InputPreset),
}

impl LanePreset {
    fn name(&self) -> &str {
        match self {
            Self::Music(preset) => &preset.name,
            Self::Voice(preset) => &preset.name,
        }
    }

    fn direction(&self) -> DeviceDirection {
        match self {
            Self::Music(_) => DeviceDirection::Output,
            Self::Voice(_) => DeviceDirection::Input,
        }
    }
}

/// What the controller asks of a lane's preset store that does not depend on the file format:
/// the list, the unsaved-changes shadow, delete, rename, import and export. Both stores are a
/// [`Store`], so both answer through the one implementation below; loading, applying and saving
/// a preset do depend on the format and go through [`LanePreset`] instead.
///
/// Errors are the store's own message: they reach the log, and the window says only which preset
/// the operation failed on.
trait LaneStore {
    fn entries(&self) -> &[fxsound_preset::PresetEntry];
    /// The extension the store's files carry, without the dot — what Import looks for.
    fn extension(&self) -> &'static str;
    /// The file `name` is kept under, and so the one an export of it writes.
    fn file_name(&self, name: &str) -> Option<String>;
    fn rescan(&mut self);
    fn clear_autosave(&mut self, name: &str);
    fn delete(&mut self, name: &str) -> Result<(), String>;
    /// Save the preset under `new` and remove `old`, as the original does through its preset list
    /// rather than a filesystem rename (`FxController.cpp:1244-1276`). What moves is the saved
    /// file; an autosave under `old` goes with `old`. A failure to remove `old` once the copy
    /// exists is logged, not returned: the rename has happened.
    fn rename(&mut self, old: &str, new: &str) -> Result<(), String>;
    fn import(&mut self, source: &Path) -> Result<String, String>;
    fn export(&self, name: &str, dir: &Path) -> Result<PathBuf, String>;
}

impl<F: PresetFile> LaneStore for Store<F> {
    fn entries(&self) -> &[fxsound_preset::PresetEntry] {
        Store::entries(self)
    }

    fn extension(&self) -> &'static str {
        F::EXTENSION
    }

    fn file_name(&self, name: &str) -> Option<String> {
        Store::<F>::file_name(name).ok()
    }

    fn rescan(&mut self) {
        Store::rescan(self);
    }

    fn clear_autosave(&mut self, name: &str) {
        Store::clear_autosave(self, name);
    }

    fn delete(&mut self, name: &str) -> Result<(), String> {
        Store::delete(self, name).map_err(|err| err.to_string())
    }

    fn rename(&mut self, old: &str, new: &str) -> Result<(), String> {
        let preset = self.load_saved(old).map_err(|err| err.to_string())?;
        self.save_as(&preset, new).map_err(|err| err.to_string())?;
        if let Err(err) = Store::delete(self, old) {
            log::warn!("could not remove the old preset {old}: {err}");
        }
        Ok(())
    }

    fn import(&mut self, source: &Path) -> Result<String, String> {
        Store::import(self, source).map_err(|err| err.to_string())
    }

    fn export(&self, name: &str, dir: &Path) -> Result<PathBuf, String> {
        Store::export(self, name, dir).map_err(|err| err.to_string())
    }
}

/// A `--output` or `--input` that arrived before the device list did (see
/// [`App::select_device_when_listed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingDevice {
    lane: DeviceDirection,
    /// The name as the command line gave it, resolved once the list exists.
    name: String,
    /// A preset picked for `lane` while the name was waiting, which the device keeps when it is
    /// selected instead of bringing back the one it remembers — as it would had the list been
    /// there, the device selected first and the preset picked after it.
    preset: Option<String>,
}

/// Everything the running application owns.
pub struct App {
    /// What the views draw.
    pub state: UiState,
    /// The snapshot published to the audio thread.
    params: DspParams,
    input_params: InputDspParams,
    /// What the selected voice preset said before the Settings pane's overrides were written
    /// over it (see [`apply_microphone_settings`]).
    input_voicing: PresetVoicing,
    /// The audio thread's reason the echo canceller is not running, for the Settings pane.
    echo_cancel_detail: String,
    /// The stage ordering the selected voice preset names, as last told to the audio thread.
    /// Not part of the snapshot, because the audio thread cannot act on it in place: a chain is
    /// built, not set, and the building happens on its main loop.
    input_chain: String,
    /// The microphone's presets: the shipped voice set, read-only, and the user's own under
    /// `presets/Input/`, with their unsaved edits under `Input/AutoSave/` (0.4.0 design §1.4).
    ///
    /// A separate store from `presets`, not a second source for the same one: a `.fac` and a voice
    /// preset describe different chains, and a name can mean one thing in each.
    voice_presets: InputPresetStore,
    /// Whether a device list has ever arrived from the audio thread.
    ///
    /// The control socket answers as soon as the GUI thread is up, which is before PipeWire has
    /// finished enumerating. Until this is true, "no device is called that" and "no device list
    /// yet" are the same observation, and they call for opposite answers.
    devices_seen: bool,
    /// The last thing the audio thread said about each lane, outputs first (see
    /// [`lane_index`]), for `--status` and `--watch`.
    audio_status: [fxsound_core::AudioStatus; 2],
    /// What each lane is attached to, as the engine last said ([`AudioToUi::Attached`]); `None`
    /// while the lane has no nodes. Outputs first.
    attached: [Option<String>; 2],
    /// The device each lane was asked to attach to and the engine has not answered about yet —
    /// the user's pick, or the saved device announced at start-up. Outputs first. Shown in the
    /// lane's combo in place of what the lane is attached to (see [`lane_selection`]), because it
    /// is where the lane is going: a device list or news about the other lane arriving in the
    /// meantime does not take the pick back. The lane's next [`AudioToUi::Attached`] or error is
    /// the engine's answer and ends it, so it never outlives that answer.
    requested_device: [Option<String>; 2],
    /// The device each lane's preset goes with, outputs first: the saved device at start-up, then
    /// every device the lane attaches to. Unlike `attached` it outlives a detach, a reconnect and
    /// the wait for a lane's first attachment, so a lane that attaches to the device it was
    /// already on has not moved (see [`AudioToUi::Attached`]), and a preset picked in the meantime
    /// — `--preset` at a cold start among them — stays. `None` only for a lane with no saved
    /// device that has not attached yet.
    preset_device: [Option<String>; 2],
    /// When the per-device volumes the engine reported last are due to be written to the settings
    /// file (see [`VOLUME_SAVE_DELAY`]).
    volume_save_due: Option<Instant>,
    /// A `--output` or `--input` that arrived before the list did, waiting for it — at most one
    /// per lane, so `--output X --input Y` at login waits for both.
    pending_devices: Vec<PendingDevice>,
    /// Persisted settings, saved when they change rather than on a timer.
    settings: Settings,
    /// Factory and user presets.
    presets: PresetStore,
    /// The speakers' preset as loaded, so "undo changes" has something to go back to and a save
    /// keeps what the window has no control for.
    loaded_preset: Option<Preset>,
    /// The microphone's voice preset as loaded, for the same two reasons: a voice preset saved
    /// from the window keeps its gate, compressor, de-esser, denoiser and chain, none of which the
    /// window can move. Kept per lane, as `loaded_preset` is, so either lane can be saved or
    /// stashed whichever one the window shows.
    loaded_voice: Option<InputPreset>,
    /// The audio engine, or `None` when PipeWire could not be reached.
    engine: Option<AudioLink>,
    /// Rasterised artwork.
    pub assets: AssetCache,
    /// Whether the settings file needs writing.
    settings_dirty: bool,
    /// `false` in tests, so a test run can never write over the user's real settings file.
    persist: bool,
    /// Where Export Presets writes. A field rather than a constant so the tests can point it at
    /// a scratch directory instead of the user's documents.
    export_dir: PathBuf,
    /// The device selection last handed to the engine for each lane, outputs first, so a saved
    /// choice goes out once per appearance of that device rather than on every device list (see
    /// [`saved_device_to_announce`]).
    announced_device: [Option<String>; 2],
    /// The desktop-notification worker (`docs/spec/06-dialogs.md` §6.6).
    notifier: Notifier,
    /// Toasts are held back until construction is over: adopting the saved preset at start-up
    /// is not something to announce.
    notifications_armed: bool,
    /// The "FxSound in system tray" tip is shown once per process (`FxController.cpp:920-926`).
    tray_tip_shown: bool,
    /// Each lane's controls, outputs first ([`lane_index`]) — the design's
    /// `PerDirection<ChainControls>` (§1.4), in the `[_; 2]` form every other per-lane field here
    /// takes.
    ///
    /// The edit direction's slot is empty: its controls are in [`UiState`], live, where the views
    /// read and write them. The other lane's slot holds that lane's controls exactly as the window
    /// last left them, unsaved edits and all. Start-up enters both lanes
    /// ([`App::adopt_saved_presets`]), so from then on the lane not being edited always has its
    /// controls here; a slot is otherwise empty only for a lane an app has never shown, which
    /// only [`App::headless_for_tests`] builds.
    lane_controls: [Option<LaneControls>; 2],
    /// What changed since the last [`App::drain_events`], in the order it changed: the one source
    /// of `--watch`, the D-Bus signals and the tray (0.4.0 design §10). Filled at each mutation,
    /// never by comparing the whole state on a timer.
    events: Vec<AppEvent>,
    /// What those events have said so far, so that a mutation which leaves a thing as the stream
    /// last described it says nothing (see [`Published`]).
    published: Published,
    /// Something the tray draws changed that no event names: the theme, the language, the preset
    /// list under an unchanged selection, or sound starting or stopping on the shown lane (see
    /// [`App::take_tray_refresh`]).
    tray_stale: bool,
    /// The calibration wizard, while it is open (0.4.0 design §8): driven from
    /// [`App::poll_audio`] with the input lane's meters, drawn through [`App::calibration_view`].
    calibration: Option<CalibrationState>,
    /// The device ranking each lane's engine was last given ([`UiToAudio::SetDevicePriority`]),
    /// outputs first, so that a ranking goes out when it changes rather than with every device
    /// list (see [`crate::priority`]).
    priority_sent: [Option<Vec<String>>; 2],
    /// The system is asleep, or about to be: logind said `PrepareForSleep(true)` and has not said
    /// `false` since (U13, [`App::system_sleeping`]). Both lanes' snapshots carry it as `mute`.
    sleeping: bool,
    /// The shown lane's meters as the last poll read them, and whether they differed from the
    /// poll's before: the window paints at sixty a second only while they move (0.4.0 design
    /// §12, [`App::meters_moved`]).
    shown_meters: Meters,
    meters_moved: bool,
}

impl App {
    /// Build the application state, load the settings and presets, and adopt the saved presets.
    ///
    /// A missing sound server is not fatal: the UI comes up, says so, and keeps working, which is
    /// far more useful than refusing to start.
    ///
    /// What the audio thread says wakes the GUI thread through `waker` (0.4.0 design §12): the
    /// engine's notifications are carried to the controller by a thread of their own that wakes
    /// it for each ([`crate::wake::forward`]).
    #[must_use]
    pub fn new(engine: Option<EngineHandle>, waker: &crate::wake::Waker) -> Self {
        let settings = Settings::load();
        let mut presets = PresetStore::with_default_dirs();
        presets.rescan();
        let mut voice_presets = InputPresetStore::with_default_dirs();
        voice_presets.rescan();
        let notifier = Notifier::new(settings.hide_notifications);
        Self::start(
            settings,
            presets,
            voice_presets,
            engine.map(|engine| AudioLink::engine(engine, waker)),
            notifier,
            true,
        )
    }

    /// The start-up every run goes through, whatever the engine is: build the state from the
    /// settings, tell the engine what the settings file asks of it ([`startup_messages`]), and
    /// bring both lanes up on their saved presets, publishing both snapshots.
    fn start(
        settings: Settings,
        presets: PresetStore,
        voice_presets: InputPresetStore,
        engine: Option<AudioLink>,
        notifier: Notifier,
        persist: bool,
    ) -> Self {
        let mut app = Self {
            state: UiState {
                power: settings.power,
                theme: settings.theme_mode,
                view: settings.view,
                filter_q: settings.filter_q,
                master_gain_db: settings.master_gain,
                balance_db: settings.balance,
                volume_leveling: settings.volume_leveling,
                hide_tooltips: settings.hide_help_tooltips,
                ..UiState::default()
            },
            params: DspParams::default(),
            input_params: unvoiced_input_params(),
            input_voicing: PresetVoicing::of(&unvoiced_input_params()),
            echo_cancel_detail: String::new(),
            input_chain: fxsound_preset::input::DEFAULT_CHAIN.to_owned(),
            voice_presets,
            devices_seen: false,
            audio_status: [fxsound_core::AudioStatus::default(); 2],
            attached: [None, None],
            requested_device: [None, None],
            preset_device: DeviceDirection::ALL.map(|lane| {
                Some(settings.device_name(lane))
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            }),
            volume_save_due: None,
            pending_devices: Vec::new(),
            settings,
            presets,
            loaded_preset: None,
            loaded_voice: None,
            engine,
            assets: AssetCache::new(),
            settings_dirty: false,
            persist,
            export_dir: if persist {
                default_export_dir()
            } else {
                std::env::temp_dir().join("fxsound-app-test-export")
            },
            announced_device: [None, None],
            notifier,
            notifications_armed: false,
            tray_tip_shown: false,
            lane_controls: [None, None],
            events: Vec::new(),
            published: Published::default(),
            tray_stale: false,
            calibration: None,
            priority_sent: [None, None],
            sleeping: false,
            shown_meters: Meters::default(),
            meters_moved: false,
        };

        // What the settings file asks of the audio thread before it does anything else. See
        // [`startup_messages`].
        for message in startup_messages(&app.settings) {
            app.send(message);
        }
        app.priority_sent =
            DeviceDirection::ALL.map(|lane| Some(priority::ranking(&app.settings, lane)));

        app.adopt_saved_presets();
        app.notifications_armed = true;
        // Start-up is where the stream starts from, not news: a subscriber's first line is the
        // status document, which already says all of it.
        app.start_the_stream_here();
        app
    }

    /// An app started the way [`App::new`] starts one — from `settings`, these presets and these
    /// voice presets — against `engine`, a stand-in that records what it is told. Nothing is
    /// written to the settings file and nothing reaches the desktop's notifications; the stores
    /// write where they were pointed.
    #[doc(hidden)]
    #[must_use]
    pub fn start_for_tests(
        settings: Settings,
        presets: PresetStore,
        voices: InputPresetStore,
        engine: &FakeEngine,
    ) -> Self {
        Self::start(
            settings,
            presets,
            voices,
            Some(AudioLink::Fake(engine.clone())),
            Notifier::new(true),
            false,
        )
    }

    /// Bring **both** lanes up on the presets the settings file remembers for them.
    ///
    /// Both, not only the one the window was last editing: the engine runs every enabled lane,
    /// whichever the window edits, so a restart that loaded only the edit direction's preset left
    /// the speakers flat — every effect at zero — for anyone whose window was last on the
    /// microphone, and a microphone running beside them unvoiced. The lane off screen goes first,
    /// loaded exactly the way a first visit loads it, and its controls go to its slot of
    /// [`App::lane_controls`]; the edit direction is entered the same way, so each lane's controls
    /// are seeded from its own chain and nothing of one reaches the other. Called while toasts are
    /// still held back, so none of this is announced.
    fn adopt_saved_presets(&mut self) {
        let edit = self.settings.device_direction;
        self.state.direction = edit.other();
        self.enter_lane_for_the_first_time(edit.other());
        self.set_edit_direction(edit);
    }

    /// The palette the views should use.
    #[must_use]
    pub fn palette(&self) -> Palette {
        Palette::new(self.state.theme)
    }

    /// `true` when the audio engine is running.
    #[must_use]
    pub const fn has_audio(&self) -> bool {
        self.engine.is_some()
    }

    /// Whether the last poll found the shown lane's meters different from the poll before it:
    /// sound moving through the visualizer. The window paints at sixty a second only while they
    /// move and the visualizer has something to show (0.4.0 design §12).
    #[must_use]
    pub const fn meters_moved(&self) -> bool {
        self.meters_moved
    }

    /// When the controller next has something to do that nothing will wake it for: the notice to
    /// take down after its four seconds, the per-device volumes to write. `None` while there is
    /// nothing of the kind; the pump's keepalive covers what is left (0.4.0 design §12).
    ///
    /// A notice written straight into [`UiState::notification`] has no clock until the next poll
    /// stamps it, so it asks for that poll now.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let notice = self
            .state
            .notification
            .as_ref()
            .map(|text| match &self.state.notice_clock {
                Some((stamped, since)) if stamped == text => {
                    *since + fxsound_ui::state::NOTICE_LIFETIME
                }
                _ => Instant::now(),
            });
        notice.into_iter().chain(self.volume_save_due).min()
    }

    /// The channel the audio thread's notifications arrive on, for the headless pump to wait on
    /// — not to read from: [`App::poll_audio`] does that. `None` without an engine.
    #[must_use]
    pub fn audio_notifications(&self) -> Option<&crossbeam_channel::Receiver<AudioToUi>> {
        self.engine.as_ref().and_then(AudioLink::notifications)
    }

    /// Pull everything the audio thread has published. Call once per frame, before rendering.
    pub fn poll_audio(&mut self) {
        self.poll_audio_at(Instant::now());
    }

    /// [`App::poll_audio`] at `now`, which the tests choose so they can step a calibration run
    /// through its phases without waiting them out.
    pub(crate) fn poll_audio_at(&mut self, now: Instant) {
        // A notice is up for four seconds (0.4.0 design, §11). The views only draw it; this is
        // the one place that times it, engine or no engine.
        self.state.expire_notification(now);
        // The per-device volumes are written a while after they start moving, not on every step
        // of a mixer drag; the next flush of the settings takes them (see `VOLUME_SAVE_DELAY`).
        if self.volume_save_due.is_some_and(|due| due <= now) {
            self.volume_save_due = None;
            self.settings_dirty = true;
        }

        self.meters_moved = false;
        let Some(engine) = self.engine.as_mut() else {
            // Nothing will ever flow: a run that was started waits out its wake-up and says so.
            self.drive_calibration(now, &Meters::default());
            return;
        };

        // Each lane publishes its own meters: the picture is the edited lane's, the microphone's
        // telemetry the input lane's (see [`show_meters`]).
        let shown = engine.meters(self.state.direction);
        let microphone = engine.meters(DeviceDirection::Input);
        let was_playing = self.state.audio_active;
        show_meters(&mut self.state, &shown, &microphone);
        self.meters_moved = shown != self.shown_meters;
        self.shown_meters = shown;

        // Drained first and acted on after, in arrival order, once the engine is no longer
        // borrowed: a device list and a pick call back into the whole controller.
        let messages: Vec<AudioToUi> = std::iter::from_fn(|| engine.try_recv()).collect();
        for message in messages {
            self.receive(message);
        }

        // Sound is playing on the shown lane only while the lane is running, as its own status
        // says. The meters come from inside the lane's process callback, so a lane whose node
        // went to sleep — its last client gone in the middle of a song — leaves its last meters
        // standing, `active` and all: the visualizer froze on its last frame and was repainted
        // thirty times a second for as long as nothing played, and the logo and the tray's icon
        // stayed lit. After the messages, so a status that says the lane started counts at once.
        if !lane_running(&self.state, self.state.direction) {
            self.state.audio_active = false;
        }
        // The tray's processing icon is the logo's and the visualizer's "sound is playing", which
        // no event names: redrawn when it starts or stops, not on every buffer.
        if self.state.audio_active != was_playing {
            self.tray_stale = true;
        }

        // After the lists: selecting a device calls back into the whole controller.
        if self.devices_seen && !self.pending_devices.is_empty() {
            self.apply_pending_device();
        }

        // Last, so the wizard sees the lane as this poll left it: attached, processing, listed.
        self.drive_calibration(now, &microphone);
    }

    /// Act on one thing the audio thread said. Crate-visible so the tests of what follows from a
    /// message — `--watch`'s events among them — can say it without an engine.
    pub(crate) fn receive(&mut self, message: AudioToUi) {
        match message {
            // In arrival order, so a device that vanished in one list and came back in the next
            // is announced afresh.
            AudioToUi::Devices(devices) => {
                for message in self.adopt_device_list(devices) {
                    self.send(message);
                }
            }
            // Each lane says how it is doing on its own, so a lane's activity is never the other
            // lane's left over: `--status`, `--watch` and the strip's Floor slot read it. A lane
            // the user has switched off is never active, whatever a status that was already on its
            // way when the lane was detached says.
            AudioToUi::Status { direction, status } => {
                let processing = status.processing && self.settings.lane_enabled(direction);
                set_lane_active(&mut self.state, direction, processing);
                self.audio_status[lane_index(direction)] = status;
                self.note_audio(direction);
            }
            // What a lane is really attached to, and the engine's answer to whatever the lane was
            // asked to do: the request ends here, and the combo shows the attachment (see
            // [`lane_selection`]).
            // A lane with no nodes processes nothing, whatever its last status said; the status
            // that says so follows on the engine's next tick.
            //
            // A move to another device that nobody here asked for — the engine's own rules after
            // a hotplug, a device gone, a priority — brings that device's preset with it, as a
            // pick in the window does, and quietly, as the original's device-change handler does
            // (`FxController.cpp:1645-1658`, `setPreset(…, false)`). A move the window, the tray,
            // the command line or the announcement of the saved device asked for brought it
            // already, when it was asked for.
            //
            // "Another device" is one other than the device the lane's preset goes with
            // ([`App::preset_device`]), as upstream moves the preset only for a device other than
            // the one it already shows: a lane attaching for the first time to its saved device,
            // or coming back after a reconnect to the one it was on, has not moved, and a preset
            // picked while it waited — `--preset` at a cold start — stays.
            AudioToUi::Attached {
                direction,
                node_name,
            } => {
                if node_name.is_none() {
                    set_lane_active(&mut self.state, direction, false);
                    self.note_audio(direction);
                }
                let slot = lane_index(direction);
                let requested = self.requested_device[slot].take();
                let moved_on_its_own = node_name.is_some()
                    && node_name != self.preset_device[slot]
                    && node_name != requested;
                self.attached[slot].clone_from(&node_name);
                if node_name.is_some() {
                    self.preset_device[slot].clone_from(&node_name);
                }
                self.show_lane_selections();
                if moved_on_its_own && let Some(node_name) = node_name {
                    self.quietly(|app| app.bring_back_device_preset(direction, &node_name));
                }
            }
            AudioToUi::Disconnected { reason } => {
                // Nothing is processed while the connection is down. Each lane's next status
                // after the reconnect says when it is again.
                for direction in DeviceDirection::ALL {
                    set_lane_active(&mut self.state, direction, false);
                    self.note_audio(direction);
                }
                self.raise_notice(format!("{} {reason}", tr("Audio disconnected:")));
                // `"Output Disconnected"` (`FxController.cpp:1170`).
                self.notify(Message::output_disconnected());
            }
            AudioToUi::Error { direction, message } => {
                // An error about a lane is the engine's answer to what that lane was asked for: a
                // device it could not attach is not shown as though it had been.
                if let Some(direction) = direction
                    && self.requested_device[lane_index(direction)]
                        .take()
                        .is_some()
                {
                    self.show_lane_selections();
                }
                self.raise_notice(message);
            }
            // Something that works but that the user should know — one Bluetooth headset on both
            // lanes, which drops its music to call quality (U9). Already translated.
            AudioToUi::Warning { message, .. } => {
                self.raise_notice(message);
            }
            AudioToUi::EchoCancel { running, detail } => {
                self.state.echo_cancel_running = running;
                self.echo_cancel_detail = detail;
                self.note_echo_cancel();
            }
            AudioToUi::RememberedDefault {
                direction,
                node_name,
            } => {
                // Straight to the settings file. The audio thread's own copy dies with the
                // process, and the three ways a process dies without warning — SIGKILL, the OOM
                // killer, a power cut — are exactly the ones that leave the session default
                // naming a node that is gone.
                if self.settings.remembered_default(direction) != node_name {
                    self.settings.set_remembered_default(direction, &node_name);
                    self.settings_dirty = true;
                }
            }
            // The volume of FxSound's own node while attached to one device (U10), so the next
            // pair built for that device starts where the user left it. Remembered at once, and
            // written with the next flush once `VOLUME_SAVE_DELAY` has passed: a mixer drag
            // reports every step, and one file write per step is what that must not cost.
            AudioToUi::TargetVolume(volume) => {
                if self.settings.remember_target_volume(volume) {
                    self.volume_save_due
                        .get_or_insert_with(|| Instant::now() + VOLUME_SAVE_DELAY);
                }
            }
        }
    }

    /// Hand a control-plane request to the engine, when there is one.
    fn send(&self, message: UiToAudio) {
        if let Some(engine) = &self.engine {
            engine.send(message);
        }
    }

    /// Take a device list from the engine, and say what, if anything, the engine has to be told.
    ///
    /// Every device in it joins the priority list the first time it is seen (U4,
    /// [`priority::learn`]), and the list is shown in that order — the combos, the tray and
    /// `--next-output` all read it from here. What the engine is told, in order: a ranking that
    /// changed, the saved devices ([`App::saved_devices_for_engine`]), and last a lane's move to a
    /// newcomer that *Prioritize new output devices* put at the top of its list, which the
    /// engine's own rules, run before the list got here, could not have known to make.
    ///
    /// Each lane shows what it has been asked to attach to and the engine has not answered about
    /// yet, else what the engine says it is attached to, found again in the new list (see
    /// [`lane_selection`]). A detached lane shows nothing: that is what `Off` means.
    fn adopt_device_list(&mut self, mut devices: Vec<AudioDevice>) -> Vec<UiToAudio> {
        self.devices_seen = true;
        let attached = [self.attached[0].as_deref(), self.attached[1].as_deref()];
        let learned = priority::learn(&mut self.settings, &devices, attached);
        if learned.changed {
            self.settings_dirty = true;
        }
        priority::sort_by_rank(&mut devices, &self.settings);
        self.state.devices = devices;
        self.note_device_list();
        // Where each lane stands is settled step by step below — the list, the saved devices
        // announced, a newcomer taken — and said once, where it ends up.
        self.place_lane_selections();
        let mut messages = self.device_priority_messages();
        messages.extend(self.saved_devices_for_engine());
        for (direction, newcomer) in DeviceDirection::ALL.into_iter().zip(learned.promoted) {
            if let Some(newcomer) = newcomer {
                messages.extend(self.promote(direction, &newcomer));
            }
        }
        self.note_lane_devices();
        messages
    }

    /// The rankings the engine has not been given yet ([`priority::ranking`]), each lane's once
    /// per change.
    fn device_priority_messages(&mut self) -> Vec<UiToAudio> {
        let mut messages = Vec::new();
        for direction in DeviceDirection::ALL {
            let names = priority::ranking(&self.settings, direction);
            let sent = &mut self.priority_sent[lane_index(direction)];
            if sent.as_ref() != Some(&names) {
                *sent = Some(names.clone());
                messages.push(UiToAudio::SetDevicePriority { direction, names });
            }
        }
        messages
    }

    /// The priority list moved — a row up or down, one removed, the *Follow the system's default
    /// device* switch — so the lists the window and the tray show are put in its new order, and
    /// the engine is given the new ranking.
    ///
    /// A new ranking moves no lane by itself, as dragging a row of upstream's list moves nothing
    /// (`FxOutputPreference.cpp:238-262`): the lane's device stays until a better-ranked one
    /// arrives or it goes.
    fn device_priority_changed(&mut self) {
        let mut devices = self.state.devices.clone();
        priority::sort_by_rank(&mut devices, &self.settings);
        if devices != self.state.devices {
            // The tray lists them in this order too, and no event names an order.
            self.state.devices = devices;
            self.tray_stale = true;
        }
        self.show_lane_selections();
        for message in self.device_priority_messages() {
            self.send(message);
        }
    }

    /// Move `lane` to `newcomer`, a device seen for the first time that *Prioritize new output
    /// devices* has just put at the top of the lane's list — upstream's `updateOutputs` taking a
    /// newcomer that ranks above the current device (`FxController.cpp:1558-1584`), which ends in
    /// `setOutput` and so becomes the device the lane is saved on.
    ///
    /// Nothing for a lane that is switched off, for one already on it, or while the system's
    /// default decides: none of those is the list's to move. Quietly, as the engine's own moves
    /// are, and without touching the edit direction: the user plugged something in, and did not
    /// ask to look at it.
    fn promote(&mut self, lane: DeviceDirection, newcomer: &str) -> Option<UiToAudio> {
        if !self.settings.lane_enabled(lane)
            || self.settings.follow_system_default
            || self.attached(lane) == Some(newcomer)
        {
            return None;
        }
        self.settings.set_device_name(lane, newcomer);
        self.settings_dirty = true;
        self.announced_device[lane_index(lane)] = Some(newcomer.to_owned());
        self.note_request(lane, newcomer);
        self.place_lane_selections();
        Some(UiToAudio::SelectDevice {
            node_name: newcomer.to_owned(),
            direction: lane,
        })
    }

    /// Point each lane's combo at the device it is on (see [`lane_selection`]), and tell the
    /// stream.
    fn show_lane_selections(&mut self) {
        self.place_lane_selections();
        self.note_lane_devices();
    }

    /// [`App::show_lane_selections`] without telling the stream yet, for a path that settles the
    /// lanes in several steps and says where they end up once.
    fn place_lane_selections(&mut self) {
        for direction in DeviceDirection::ALL {
            let selection = lane_selection(
                &self.settings,
                &self.state.devices,
                direction,
                self.attached[lane_index(direction)].as_deref(),
                self.requested_device[lane_index(direction)].as_deref(),
            );
            self.state.set_selection(direction, selection);
        }
    }

    /// Record that `lane` has been asked to attach to `node_name`, so its combo can show the
    /// request until the engine answers. Asking for what the lane is already attached to needs no
    /// answer, and the engine sends none.
    fn note_request(&mut self, lane: DeviceDirection, node_name: &str) {
        let slot = lane_index(lane);
        self.requested_device[slot] =
            (self.attached[slot].as_deref() != Some(node_name)).then(|| node_name.to_owned());
    }

    /// The engine starts with the output lane enabled and attached by the device rules alone, and
    /// the input lane detached; each enabled lane's saved choice reaches it from here, the first
    /// time that device is listed (see [`saved_device_to_announce`]). Outputs first.
    ///
    /// Never the edit direction: that says which chain the window shows, and looking at the other
    /// lane's list must not change what the engine runs (0.4.0 design, §1.1) — least of all
    /// attach a lane the user switched off.
    ///
    /// Each announcement is also a request the lane's combo shows until the engine answers it, so
    /// a microphone brought back at start-up reads as that microphone rather than `Off` for the
    /// moment before its lane is attached.
    fn saved_devices_for_engine(&mut self) -> Vec<UiToAudio> {
        let messages: Vec<UiToAudio> = DeviceDirection::ALL
            .into_iter()
            .filter_map(|direction| {
                saved_device_to_announce(
                    &self.settings,
                    &self.state.devices,
                    direction,
                    &mut self.announced_device[lane_index(direction)],
                )
            })
            .collect();
        let mut requested = false;
        for message in &messages {
            if let UiToAudio::SelectDevice {
                node_name,
                direction,
            } = message
            {
                self.note_request(*direction, node_name);
                requested = true;
            }
        }
        if requested {
            self.place_lane_selections();
        }
        // The saved device comes back with the preset it remembers, as a device the engine moves
        // to does (see `AudioToUi::Attached`), when the lane really was on another device: the
        // headphones' own preset when they are plugged back in, or when the engine's rules
        // attached the lane to the speakers before the list came. A lane still on, or still
        // waiting for, the device its preset goes with ([`App::preset_device`]) keeps its preset:
        // at an ordinary start that is the saved one, or whatever was picked before the list came
        // — `--preset` at a cold start among them — which the saved device must not overwrite.
        for message in &messages {
            if let UiToAudio::SelectDevice {
                node_name,
                direction,
            } = message
                && self.preset_device[lane_index(*direction)]
                    .as_ref()
                    .is_some_and(|was| was != node_name)
            {
                self.quietly(|app| app.bring_back_device_preset(*direction, node_name));
            }
        }
        messages
    }

    /// Do `act` with the desktop notifications held back: for a preset the controller moves on
    /// the engine's word rather than the user's, which the original moves with
    /// `setPreset(…, false)` (`FxController.cpp:1645-1658`).
    fn quietly(&mut self, act: impl FnOnce(&mut Self)) {
        let armed = std::mem::replace(&mut self.notifications_armed, false);
        act(self);
        self.notifications_armed = armed;
    }

    /// Act on everything the views reported this frame.
    pub fn handle(&mut self, actions: &[UiAction]) {
        for action in actions {
            self.handle_one(action.clone());
        }
        self.save_settings_if_dirty();
    }

    /// Write the settings file if anything in it changed — once per batch of actions rather than
    /// once per action.
    fn save_settings_if_dirty(&mut self) {
        if self.settings_dirty {
            if self.persist
                && let Err(err) = self.settings.save()
            {
                log::warn!("could not save settings: {err}");
            }
            self.settings_dirty = false;
        }
    }

    fn handle_one(&mut self, action: UiAction) {
        match action {
            UiAction::TogglePower => {
                self.state.power = !self.state.power;
                self.settings.power = self.state.power;
                self.settings_dirty = true;
                self.note_power();
                self.sync_params_from_state();
                // Off hands the session defaults back to the real devices, so the system's sound
                // no longer passes through FxSound at all; on takes them again (U12, upstream
                // `powerOn` → `restoreDefaultPlaybackDevice`, `FxController.cpp:1727-1748`). The
                // combos follow: while off they show the system's default device.
                for message in default_claims(self.state.power) {
                    self.send(message);
                }
                self.show_lane_selections();
                // Coming back from a bypass, the filters still hold whatever was in them when the
                // power went off, and `Chain::set_power` clears only the five effects — never the
                // equalizer or the leveller. That also makes the power button the one recovery a
                // user with broken-sounding audio will reach for first, so it has to be the one
                // that actually clears the history. Both the tray and `--power` route here.
                // Power is one switch over both chains, so both lanes are reset.
                if self.state.power
                    && let Some(engine) = &self.engine
                {
                    for direction in DeviceDirection::ALL {
                        engine.send_event(direction, DspEvent::ResetFilterState);
                    }
                }
            }
            UiAction::ToggleView => {
                self.state.view = match self.state.view {
                    ViewMode::Pro => ViewMode::Lite,
                    ViewMode::Lite => ViewMode::Pro,
                };
                self.settings.view = self.state.view;
                self.settings_dirty = true;
            }
            UiAction::ToggleTheme => {
                self.state.theme = match self.state.theme {
                    ThemeMode::Dark => ThemeMode::Light,
                    ThemeMode::Light => ThemeMode::Dark,
                };
                self.settings.theme_mode = self.state.theme;
                self.settings_dirty = true;
                // The artwork differs per theme, so the old textures will never be used again.
                self.assets.clear();
                // The tray's Theme items tick the one in force.
                self.tray_stale = true;
            }

            UiAction::SelectPreset(index) => {
                self.select_preset(index);
                self.hold_picked_preset();
            }
            UiAction::SavePreset => self.save_preset(None),
            UiAction::SavePresetAs(name) => self.save_preset(Some(name)),
            UiAction::UndoPresetChanges => self.undo_preset_changes(),
            UiAction::DeletePreset => self.delete_preset(),

            UiAction::SelectDevice(index) => {
                // The tray and the command line name a device, not a lane: the device's own
                // direction says which lane it is for.
                if let Some(direction) = self.state.devices.get(index).map(|d| d.direction) {
                    self.select_device(index, direction);
                }
            }
            UiAction::SelectOutput(index) => self.select_device(index, DeviceDirection::Output),
            UiAction::SelectInput(index) => self.select_device(index, DeviceDirection::Input),
            UiAction::DetachOutput => self.detach_lane(DeviceDirection::Output),
            UiAction::DetachInput => self.detach_lane(DeviceDirection::Input),
            UiAction::SetEditDirection(direction) => {
                self.set_edit_direction(direction);
            }
            UiAction::DismissNotice => self.state.dismiss_notification(),

            UiAction::SetEffect(effect, value) => {
                self.state.effects[effect as usize] = value.clamp(0.0, scale::SLIDER_MAX);
                self.mark_preset_modified();
                self.sync_params_from_state();
            }
            UiAction::SetBandGain(band, gain_db) => {
                if let Some(slot) = self.state.eq_bands.get_mut(band) {
                    slot.boost_db =
                        gain_db.clamp(fxsound_core::eq::MIN_GAIN_DB, fxsound_core::eq::MAX_GAIN_DB);
                    self.mark_preset_modified();
                    self.sync_params_from_state();
                }
            }
            UiAction::SetBandFrequency(band, hz) => {
                if let Some(slot) = self.state.eq_bands.get_mut(band) {
                    slot.center_hz = hz;
                    self.mark_preset_modified();
                    self.sync_params_from_state();
                }
            }
            UiAction::SetEqEnabled(on) => {
                self.state.eq_on = on;
                self.mark_preset_modified();
                self.sync_params_from_state();
            }
            UiAction::SetBandCount(count) => {
                self.set_band_count(count);
            }
            UiAction::SetFilterQ(q) => {
                self.state.filter_q = q.clamp(1.0, 3.0);
                // The four level settings are the music chain's. On a microphone the same controls
                // are the voice preset's, and writing them here would hand the speakers a voice's
                // numbers the next time they are edited.
                if self.state.direction == DeviceDirection::Output {
                    self.settings.filter_q = self.state.filter_q;
                    self.settings_dirty = true;
                }
                self.sync_params_from_state();
            }
            UiAction::RestoreDefaults => self.restore_defaults(),

            UiAction::SetMasterGain(db) => {
                self.state.master_gain_db = db.clamp(-20.0, 20.0);
                match self.state.direction {
                    // The speakers' master gain is a setting over every `.fac`, as the original's.
                    DeviceDirection::Output => {
                        self.settings.master_gain = self.state.master_gain_db;
                        self.settings_dirty = true;
                    }
                    // On a voice it is the preset's own makeup, which a save writes into the
                    // preset: moving it is an edit to the preset like an equalizer move.
                    DeviceDirection::Input => self.mark_preset_modified(),
                }
                self.sync_params_from_state();
            }
            UiAction::SetBalance(db) => {
                self.state.balance_db = db.clamp(-20.0, 20.0);
                if self.state.direction == DeviceDirection::Output {
                    self.settings.balance = self.state.balance_db;
                    self.settings_dirty = true;
                }
                self.sync_params_from_state();
            }
            UiAction::SetVolumeLeveling(amount) => {
                self.state.volume_leveling = amount.clamp(0.0, 4.0);
                if self.state.direction == DeviceDirection::Output {
                    self.settings.volume_leveling = self.state.volume_leveling;
                    self.settings_dirty = true;
                }
                self.sync_params_from_state();
            }

            // These are window-level concerns the shell deals with; the controller only records
            // them so a headless test can assert they were emitted.
            UiAction::OpenSettings
            | UiAction::OpenMenu
            | UiAction::Minimise
            | UiAction::Close
            | UiAction::DragWindow => {}
        }
    }

    /// Step to the next or previous preset, which is what the compositor keybinds do.
    pub fn cycle_preset(&mut self, forward: bool) {
        let next = if forward {
            self.state.next_preset()
        } else {
            self.state.previous_preset()
        };
        if let Some(index) = next {
            self.select_preset(index);
            self.hold_picked_preset();
        }
    }

    /// Rebuild the list the picker shows, from the edit direction's store.
    ///
    /// The two sets are never merged. A music preset on a microphone is wrong by construction, and
    /// a list mixing both would make picking the wrong one a normal thing to do. Both lists come
    /// from a store the same way, so a voice preset is factory or the user's, and carries the
    /// unsaved-changes marker of an autosave, exactly as a `.fac` does.
    fn refresh_preset_list(&mut self) {
        self.state.presets = self
            .store(self.state.direction)
            .entries()
            .iter()
            .map(|entry| PresetEntry {
                name: entry.name.clone(),
                factory: entry.source == fxsound_preset::PresetSource::Factory,
                modified: entry.modified,
            })
            .collect();
        // The tray lists them. A list that grew under the same selection — an import — is not
        // an event, and the selection is noted by whoever moves it.
        self.tray_stale = true;
    }

    /// `lane`'s preset store: the `.fac` set for the speakers, the voice set for the microphone.
    fn store(&self, lane: DeviceDirection) -> &dyn LaneStore {
        match lane {
            DeviceDirection::Output => &self.presets,
            DeviceDirection::Input => &self.voice_presets,
        }
    }

    fn store_mut(&mut self, lane: DeviceDirection) -> &mut dyn LaneStore {
        match lane {
            DeviceDirection::Output => &mut self.presets,
            DeviceDirection::Input => &mut self.voice_presets,
        }
    }

    /// Put a voice preset's settings into the interface and publish them.
    ///
    /// The four stage switches live in [`UiState`] rather than in the mapping precisely so that
    /// this can move them: without it a preset's gate is a number nothing reads.
    fn apply_input_preset(&mut self, preset: &InputPreset) {
        let params = preset.to_params();
        self.state.eq_on = params.eq_on;
        self.state.eq_bands = params
            .bands()
            .0
            .iter()
            .zip(params.bands().1)
            .map(|(&center_hz, &boost_db)| fxsound_core::EqBand {
                center_hz,
                boost_db,
            })
            .collect();
        self.state.filter_q = params.filter_q;
        self.state.master_gain_db = params.makeup_db;
        self.state.denoise_on = params.rnnoise;
        self.state.gate_on = params.gate_on;
        self.state.compressor_on = params.compressor_on;
        self.state.deesser_on = params.deesser_on;

        // The stages the interface has no control for come straight from the preset, which is the
        // whole reason the voice set is navigated by preset rather than by knobs.
        self.input_voicing = PresetVoicing::of(&params);
        self.input_params = params;
        self.sync_params_from_state();

        // The chain goes with the settings, and separately from them: it is the one thing in a
        // voice preset the snapshot cannot carry (see `input_chain`). Sent even when the name has
        // not changed — the audio thread treats a repeat as a no-op, and the alternative is a
        // second copy of its knowledge here that could drift from it.
        self.input_chain.clone_from(&preset.chain);
        if let Some(engine) = &self.engine {
            engine.send(UiToAudio::SetInputChain(preset.chain.clone()));
        }
    }

    /// Select the edit direction's preset `index`: load it from that lane's store — its autosave
    /// when it has unsaved edits — and put it in the window and the lane's snapshot.
    ///
    /// The same steps on both lanes, as the original's `setPreset` (`FxController.cpp:1048-1104`):
    /// switching away from unsaved edits stashes them first, so nothing the user did is silently
    /// lost; the device in use remembers the preset; and the lane's filter history is cleared,
    /// since a new band layout makes the old one meaningless.
    fn select_preset(&mut self, index: usize) {
        let Some(entry) = self.state.presets.get(index) else {
            return;
        };
        let name = entry.name.clone();
        let lane = self.state.direction;

        if let Some(current) = self.state.preset()
            && current.modified
            && current.name != name
            && let Some(preset) = self.current_preset_snapshot()
        {
            self.autosave_lane_preset(&preset);
        }

        let loaded = match lane {
            DeviceDirection::Output => self.presets.load(&name).map_or_else(
                |err| Err(err.to_string()),
                |(preset, from_autosave)| {
                    self.apply_preset(&preset);
                    self.loaded_preset = Some(preset);
                    Ok(from_autosave)
                },
            ),
            DeviceDirection::Input => self.voice_presets.load(&name).map_or_else(
                |err| Err(err.to_string()),
                |(preset, from_autosave)| {
                    self.apply_input_preset(&preset);
                    self.loaded_voice = Some(preset);
                    Ok(from_autosave)
                },
            ),
        };
        match loaded {
            Ok(from_autosave) => {
                self.state.selected_preset = Some(index);
                if let Some(entry) = self.state.presets.get_mut(index) {
                    entry.modified = from_autosave;
                }
                // `"Preset: "` + name on every change (`FxController.cpp:1101`).
                self.notify(Message::preset_selected(&name));
                self.remember_preset_for_selected_device(&name);
                // Recorded against the lane rather than through the edit direction: start-up
                // loads the lane off screen through here too, and the two are the same by now.
                self.settings.set_preset_for_direction(lane, &name);
                self.settings_dirty = true;
                if let Some(engine) = &self.engine {
                    engine.send_event(lane, DspEvent::ResetFilterState);
                }
            }
            Err(err) => {
                log::warn!("could not load preset {name}: {err}");
                self.raise_notice(tr_args("Could not load %s", &[name.as_str()]));
            }
        }
        self.note_presets();
    }

    /// Record `preset` against whatever is playing, so plugging the headphones back in brings this
    /// preset with them. `DeviceConfig` and its two accessors were written and tested for exactly
    /// this and then never called by anything.
    fn remember_preset_for_selected_device(&mut self, preset: &str) {
        if let Some((node, description, form_factor, direction)) =
            self.state.selected_device().and_then(|at| {
                self.state.devices.get(at).map(|d| {
                    (
                        d.name.clone(),
                        d.description.clone(),
                        d.form_factor.clone(),
                        d.direction,
                    )
                })
            })
        {
            self.settings.remember_device_preset(
                &node,
                &description,
                preset,
                &form_factor,
                direction,
            );
            self.settings_dirty = true;
        }
    }

    /// Load the preset `node_name` remembers into `lane`, whichever lane the window edits — for a
    /// device the lane has moved to without a pick in the window, and for the Output Device
    /// Preference row of the device playing now. Nothing happens when the device remembers
    /// nothing, when what it remembers is no longer a preset, or when that preset is already the
    /// lane's.
    fn bring_back_device_preset(&mut self, lane: DeviceDirection, node_name: &str) {
        let Some(preset) = self
            .settings
            .preset_for_device(node_name, lane)
            .map(ToOwned::to_owned)
        else {
            return;
        };
        self.in_lane(lane, |app| {
            if app.state.preset().is_some_and(|p| p.name == preset) {
                return;
            }
            if let Some(at) = app.state.presets.iter().position(|p| p.name == preset) {
                app.select_preset(at);
            }
        });
    }

    /// Where the preset the device of the lane on screen remembers sits in the window's list;
    /// `None` when the lane has no device, the device remembers nothing, or what it remembers is
    /// no longer a preset. What Delete Preset and Reset Presets fall back to (upstream 7f160b6).
    fn device_preset_index(&self) -> Option<usize> {
        let lane = self.state.direction;
        let device = self.state.device_for(lane)?;
        let preset = self.settings.preset_for_device(&device.name, lane)?;
        self.state.presets.iter().position(|p| p.name == preset)
    }

    /// Put a music preset in the window and publish it — its curve on the user's band count, not
    /// its own (upstream `DfxDspEq.cpp:127-247`).
    ///
    /// A user on thirty-one bands who picks a ten-band factory preset stays on thirty-one bands:
    /// the live ladder is kept and the preset's gains are fitted onto it by position
    /// ([`fxsound_dsp::eq::fit_preset_gains`]). Only a preset of the live count brings its own
    /// centre frequencies. A preset with no equalizer at all is the original's "old preset": the
    /// equalizer on and flat.
    fn apply_preset(&mut self, preset: &Preset) {
        for effect in Effect::ALL {
            self.state.effects[effect as usize] =
                scale::value_to_slider_for(effect, preset.effect(effect));
        }
        let ladder = self.music_ladder();
        (self.state.eq_on, self.state.eq_bands) = if preset.eq_bands.is_empty() {
            (true, bands_of(&ladder, &vec![0.0; ladder.len()]))
        } else if preset.eq_bands.len() == ladder.len() {
            (preset.eq_on, preset.eq_bands.clone())
        } else {
            let centres: Vec<f32> = preset.eq_bands.iter().map(|b| b.center_hz).collect();
            let gains: Vec<f32> = preset.eq_bands.iter().map(|b| b.boost_db).collect();
            let fitted = fxsound_dsp::eq::fit_preset_gains(&centres, &gains, &ladder);
            (preset.eq_on, bands_of(&ladder, &fitted))
        };
        self.sync_params_from_state();
    }

    /// The music lane's live band ladder: the user's band count (`settings.num_bands`), at the
    /// centres the window holds when it holds that many bands, else at the engine's own ladder
    /// for the count. Asked while the window shows the music lane.
    fn music_ladder(&self) -> Vec<f32> {
        let count = (self.settings.num_bands as usize).clamp(1, fxsound_core::eq::MAX_BANDS);
        if self.state.eq_bands.len() == count {
            self.state.eq_bands.iter().map(|b| b.center_hz).collect()
        } else {
            ladder(count)
        }
    }

    /// The edit direction's controls as that lane's kind of preset, under the selected preset's
    /// name, ready to save: a `.fac` on the speakers, a TOML voice preset on the microphone.
    /// `None` with no preset selected.
    fn current_preset_snapshot(&self) -> Option<LanePreset> {
        self.lane_snapshot(self.state.direction)
            .map(|(preset, _)| preset)
    }

    /// `lane`'s controls as that lane's kind of preset, and whether they carry unsaved changes —
    /// read wherever the controls are: in [`UiState`] while the window edits the lane, in the
    /// lane's slot of [`App::lane_controls`] while it edits the other one.
    ///
    /// Each lane's controls become only its own kind of file. A microphone's equalizer filed as a
    /// `.fac` under the voice preset's name — 0.3.0's save and autosave on a microphone — is the
    /// voice lane leaking into the music store, and can overwrite the autosave of a music preset
    /// that shares the name.
    fn lane_snapshot(&self, lane: DeviceDirection) -> Option<(LanePreset, bool)> {
        let (entry, effects, eq_bands, eq_on, gain_db) = if lane == self.state.direction {
            let state = &self.state;
            (
                state.preset()?,
                &state.effects,
                state.eq_bands.as_slice(),
                state.eq_on,
                state.master_gain_db,
            )
        } else {
            let controls = self.lane_controls[lane_index(lane)].as_ref()?;
            let entry = controls
                .selected_preset
                .and_then(|i| controls.presets.get(i))?;
            (
                entry,
                &controls.effects,
                controls.eq_bands.as_slice(),
                controls.eq_on,
                controls.master_gain_db,
            )
        };
        let name = entry.name.clone();
        let preset = match lane {
            DeviceDirection::Output => {
                LanePreset::Music(self.music_preset_from(name, effects, eq_bands, eq_on))
            }
            DeviceDirection::Input => {
                LanePreset::Voice(self.voice_preset_from(name, eq_bands, eq_on, gain_db)?)
            }
        };
        Some((preset, entry.modified))
    }

    /// A music lane's controls written over the `.fac` they were loaded from, under `name`.
    fn music_preset_from(
        &self,
        name: String,
        effects: &[f32; Effect::COUNT],
        eq_bands: &[EqBand],
        eq_on: bool,
    ) -> Preset {
        let mut preset = self.loaded_preset.clone().unwrap_or_default();
        preset.name = name;
        for effect in Effect::ALL {
            preset.set_effect(
                effect,
                scale::slider_to_value_for(effect, effects[effect as usize]),
            );
        }
        preset.eq_bands = eq_bands.to_vec();
        preset.eq_on = eq_on;
        preset
    }

    /// A voice lane's controls written over the voice preset they were loaded from, under `name`.
    ///
    /// Only what the window can move on a voice is written: the equalizer — its switch, its
    /// ladder and its curve — and the output gain, which is the chain's makeup. Everything else
    /// is the preset's and is kept as loaded: the high-pass, the gate, compressor and de-esser
    /// tables, the denoiser, the de-reverb and the chain. In particular a noise-suppression level
    /// pinned in Settings is a setting over every preset, not part of this one, so it is never
    /// written into the file (the window's denoiser switch shows the level in force, which is why
    /// it is not read here). `None` when no voice preset was loaded to write over: a preset is
    /// never made up from defaults.
    fn voice_preset_from(
        &self,
        name: String,
        eq_bands: &[EqBand],
        eq_on: bool,
        makeup_db: f32,
    ) -> Option<InputPreset> {
        let mut preset = self.loaded_voice.clone()?;
        preset.name = name;
        preset.eq.centers_hz = eq_bands.iter().map(|band| band.center_hz).collect();
        preset.eq.gains_db = eq_bands.iter().map(|band| band.boost_db).collect();
        preset.eq.enabled = eq_on;
        preset.makeup_db = makeup_db;
        Some(preset)
    }

    /// `lane`'s unsaved edits as that lane's kind of preset, for the autosave on the way out.
    /// `None` when its selected preset carries no unsaved changes.
    fn unsaved_lane_preset(&self, lane: DeviceDirection) -> Option<LanePreset> {
        self.lane_snapshot(lane)
            .and_then(|(preset, modified)| modified.then_some(preset))
    }

    /// Stash unsaved edits in their own lane's store, so they survive a preset switch or a
    /// restart. A failure is logged: the edits are still in the window, and nothing the user
    /// asked for has failed.
    fn autosave_lane_preset(&self, preset: &LanePreset) {
        let result = match preset {
            LanePreset::Music(preset) => self.presets.autosave(preset).map_err(|e| e.to_string()),
            LanePreset::Voice(preset) => self
                .voice_presets
                .autosave(preset)
                .map_err(|e| e.to_string()),
        };
        if let Err(err) = result {
            log::warn!("could not autosave {}: {err}", preset.name());
        }
    }

    /// Save `preset` as a user preset called `name` in its own lane's store — the overwrite when
    /// the name is taken — and make it what that lane's undo goes back to.
    fn save_lane_preset(&mut self, preset: LanePreset, name: &str) -> Result<(), String> {
        match preset {
            LanePreset::Music(mut preset) => {
                self.presets
                    .save_as(&preset, name)
                    .map_err(|e| e.to_string())?;
                name.clone_into(&mut preset.name);
                self.loaded_preset = Some(preset);
            }
            LanePreset::Voice(mut preset) => {
                self.voice_presets
                    .save_as(&preset, name)
                    .map_err(|e| e.to_string())?;
                name.clone_into(&mut preset.name);
                self.loaded_voice = Some(preset);
            }
        }
        Ok(())
    }

    fn mark_preset_modified(&mut self) {
        if let Some(index) = self.state.selected_preset
            && let Some(entry) = self.state.presets.get_mut(index)
        {
            entry.modified = true;
        }
        self.note_presets();
    }

    /// Save the edit direction's controls: over the selected preset (`None`), or as a new user
    /// preset called `new_name` — into that lane's store, as that lane's kind of file.
    fn save_preset(&mut self, new_name: Option<String>) {
        let Some(preset) = self.current_preset_snapshot() else {
            return;
        };
        let lane = preset.direction();
        let is_new = new_name.is_some();
        let name = new_name.unwrap_or_else(|| preset.name().to_owned());

        match self.save_lane_preset(preset, &name) {
            Ok(()) => {
                self.refresh_preset_list();
                self.state.selected_preset = self.state.presets.iter().position(|p| p.name == name);
                self.settings.set_preset_for_direction(lane, &name);
                // The selection moved to what was saved, so that is what the device in use brings
                // back next time, as it would had the preset been picked.
                self.remember_preset_for_selected_device(&name);
                self.settings_dirty = true;
                // `FxController.cpp:1221` / `:1234`, the same text on the desktop and in the strip.
                let message = if is_new {
                    Message::preset_saved(&name)
                } else {
                    Message::preset_overwritten(&name)
                };
                self.note_presets();
                self.raise_notice(message.body.clone());
                self.notify(message);
            }
            Err(err) => {
                log::warn!("could not save preset {name}: {err}");
                self.raise_notice(tr_args("Could not save %s", &[name.as_str()]));
            }
        }
    }

    /// Drop the selected preset's unsaved edits and load it as saved, on either lane
    /// (`FxController.cpp:1317-1332`).
    fn undo_preset_changes(&mut self) {
        let Some(index) = self.state.selected_preset else {
            return;
        };
        let Some(name) = self.state.presets.get(index).map(|p| p.name.clone()) else {
            return;
        };
        self.store_mut(self.state.direction).clear_autosave(&name);
        self.select_preset(index);
    }

    /// Delete the selected user preset from the edit direction's store; a factory preset refuses.
    fn delete_preset(&mut self) {
        let Some(index) = self.state.selected_preset else {
            return;
        };
        let Some(entry) = self.state.presets.get(index) else {
            return;
        };
        if entry.factory {
            self.raise_notice(tr("Factory presets cannot be deleted"));
            return;
        }
        let name = entry.name.clone();
        match self.store_mut(self.state.direction).delete(&name) {
            Ok(()) => {
                self.refresh_preset_list();
                // The device's own preset, as the original does (upstream 7f160b6); else the
                // neighbour, so the list does not jump back to its top.
                let next = self
                    .device_preset_index()
                    .unwrap_or_else(|| index.min(self.state.presets.len().saturating_sub(1)));
                if self.state.presets.is_empty() {
                    self.state.selected_preset = None;
                } else {
                    self.select_preset(next);
                }
                self.note_presets();
                let message = Message::preset_deleted(&name);
                self.raise_notice(message.body.clone());
                self.notify(message);
            }
            Err(err) => {
                log::warn!("could not delete preset {name}: {err}");
                self.raise_notice(tr_args("Could not delete %s", &[name.as_str()]));
            }
        }
    }

    /// Change the edit direction's band count, carrying the curve over by position rather than
    /// wiping it flat (upstream 182a329, `GraphicEqSet.cpp:200-245`): the window, `--num_bands`
    /// and Restore Defaults all come through here.
    ///
    /// The new ladder is the engine's own for the count. On the speakers the count is the user's
    /// setting, which every music preset is then fitted onto ([`App::apply_preset`]); on a
    /// microphone it is the voice preset's own, as its ladder is, and the setting is left alone,
    /// as the four level settings are.
    fn set_band_count(&mut self, count: usize) {
        let count = count.clamp(1, fxsound_core::eq::MAX_BANDS);
        if count == self.state.eq_bands.len() {
            return;
        }
        let gains: Vec<f32> = self.state.eq_bands.iter().map(|b| b.boost_db).collect();
        let gains = fxsound_dsp::eq::remap_band_gains(&gains, count);
        self.state.eq_bands = bands_of(&ladder(count), &gains);
        if self.state.direction == DeviceDirection::Output {
            self.settings.num_bands = count as u32;
            self.settings_dirty = true;
        }
        self.mark_preset_modified();
        self.sync_params_from_state();
        // The new ladder is the edited chain's; the other lane keeps its own curve until it is
        // edited, so only this lane's filter history is stale.
        if let Some(engine) = &self.engine {
            engine.send_event(self.state.direction, DspEvent::ResetFilterState);
        }
    }

    /// Restore Defaults under the equalizer (`FxEqualizerControl::restoreDefaults`,
    /// `FxAudioControls.cpp:529-544`): ten bands, no volume leveling, centred balance, the
    /// narrowest filter width and no master gain. The curve is kept — carried onto ten bands
    /// ([`App::set_band_count`]) — since it is the preset's, not a default.
    ///
    /// Each value goes where its own control sends it: on the speakers the levels are settings
    /// over every preset; on a microphone the gain is the voice preset's makeup, so moving it is
    /// an edit to the preset.
    fn restore_defaults(&mut self) {
        self.set_band_count(fxsound_core::eq::DEFAULT_BANDS);
        let makeup_moved = self.state.master_gain_db != 0.0;
        self.state.filter_q = 1.0;
        self.state.master_gain_db = 0.0;
        self.state.balance_db = 0.0;
        self.state.volume_leveling = 0.0;
        match self.state.direction {
            DeviceDirection::Output => {
                self.settings.filter_q = 1.0;
                self.settings.master_gain = 0.0;
                self.settings.balance = 0.0;
                self.settings.volume_leveling = 0.0;
                self.settings_dirty = true;
            }
            DeviceDirection::Input if makeup_moved => self.mark_preset_modified(),
            DeviceDirection::Input => {}
        }
        self.sync_params_from_state();
    }

    /// Rebuild the DSP snapshot from the UI state and publish it.
    ///
    /// Called after every change rather than on a timer: publishing is wait-free, so there is no
    /// reason to batch it, and a slider drag should be audible immediately.
    ///
    /// Only the **edit direction's** snapshot is written from the window; the other lane's keeps
    /// what it was last given, because the window is not showing it. The power switch is the one
    /// control both share, and it reaches both.
    fn sync_params_from_state(&mut self) {
        self.params.power = self.state.power;
        self.input_params.power = self.state.power;
        // Silence after both chains while the system sleeps (U13, [`App::system_sleeping`]):
        // whatever else a snapshot changes, it cannot unmute a system on its way to sleep.
        self.params.mute = self.sleeping;
        self.input_params.mute = self.sleeping;
        match self.state.direction {
            DeviceDirection::Output => self.sync_output_params_from_state(),
            DeviceDirection::Input => self.sync_input_params_from_state(),
        }
        // Whichever lane the window shows: the Settings pane's microphone settings are global, and
        // changing one while the speakers are being edited must still reach the voice chain.
        apply_microphone_settings(&mut self.input_params, &self.input_voicing, &self.settings);
        self.reflect_input_params();

        if let Some(engine) = self.engine.as_mut() {
            engine.set_params(self.params);
            engine.set_input_params(self.input_params);
        }
    }

    /// The window's controls, mapped onto the music chain.
    fn sync_output_params_from_state(&mut self) {
        for effect in Effect::ALL {
            self.params.set_effect(
                effect,
                scale::slider_to_value_for(effect, self.state.effects[effect as usize]),
            );
        }
        self.params.eq_on = self.state.eq_on;
        self.params.set_bands(&self.state.eq_bands);
        self.params.filter_q = self.state.filter_q;
        self.params.master_gain_db = self.state.master_gain_db;
        self.params.balance = self.state.balance_db;
        self.params.volume_leveling_db = self.state.volume_leveling;
    }

    /// What the readout strip reports about the voice chain that the meters do not: the level in
    /// force, whether the de-reverb and the echo canceller are asked for, and the de-esser corner
    /// the preset asked for (so the strip can tell when the adaptive mode moved it).
    fn reflect_input_params(&mut self) {
        // The switch in force, not only the preset's: a pinned noise-suppression level runs the
        // denoiser on a preset that has none, and the strip has to say so.
        self.state.denoise_on = self.input_params.rnnoise;
        self.state.denoise_level = self.input_params.denoise_level;
        self.state.dereverb_on = self.input_params.dereverb != fxsound_core::DereverbLevel::Off;
        self.state.deesser_requested_hz = self.input_params.deesser_hz;
        self.state.echo_cancel_on = self.settings.echo_cancel;
        self.note_echo_cancel();
    }

    /// Attach `lane` to device `index` — the one path the window, the tray and the command line
    /// all take.
    ///
    /// The other lane is left exactly as it is: picking a microphone no longer means the speakers
    /// went away. Picking a device also makes its lane the edit direction, since that is the chain
    /// the user has just shown an interest in.
    fn select_device(&mut self, index: usize, lane: DeviceDirection) {
        self.select_device_keeping(index, lane, None);
    }

    /// [`App::select_device`], with `picked` — a preset chosen for `lane` while the device was
    /// waiting for the list (see [`PendingDevice`]) — in place of the one the device remembers.
    fn select_device_keeping(
        &mut self,
        index: usize,
        lane: DeviceDirection,
        picked: Option<String>,
    ) {
        // Everything needed from the device is taken before anything borrows `self` mutably,
        // because restoring the remembered preset calls back into `select_preset`.
        let Some((name, description, direction)) = self
            .state
            .devices
            .get(index)
            .map(|d| (d.name.clone(), d.description.clone(), d.direction))
        else {
            return;
        };
        if direction != lane {
            log::warn!("{name} is not an {lane:?} device; not selected");
            return;
        }
        // Read before anything below records a preset against this device.
        let remembered = self
            .settings
            .preset_for_device(&name, direction)
            .map(ToOwned::to_owned);

        // The pick, shown at once — and the device a preset picked below is remembered against,
        // whatever the combo ends up showing (see the end of this function).
        self.state.set_selection(direction, Some(index));
        if self.state.power {
            self.note_lane_devices();
        }
        self.settings.set_device_name(direction, &name);
        // Picking a device switches its lane on, and says nothing about the other lane: the
        // engine runs both, and choosing speakers leaves the microphone as it was. Recorded so
        // the next start attaches it again.
        self.settings.set_lane_enabled(direction, true);
        self.settings_dirty = true;
        // The interface follows the device immediately rather than waiting for the next device
        // list: picking a microphone is exactly the moment the five effect sliders stop meaning
        // anything, and a frame of them still looking live is a frame of lying.
        self.set_edit_direction(direction);
        if let Some(engine) = &self.engine {
            engine.send(UiToAudio::SelectDevice {
                node_name: name.clone(),
                direction,
            });
            self.announced_device[lane_index(direction)] = Some(name.clone());
        }
        // Shown from now on, whatever the lane is attached to, and kept across a device list or
        // news about the other lane that arrives before the engine has answered; the lane's own
        // `Attached` (or error) is what the combo shows after that.
        self.note_request(direction, &name);

        // A device the user has used before brings its preset back with it. Only a *remembered*
        // one does: the first time something is plugged in, whatever is selected stays selected,
        // because guessing then would be changing the sound on no evidence at all. (Moving to the
        // other lane already brought back what that lane last had, in `set_edit_direction`.)
        //
        // A preset picked while the device waited for the list is the later word and wins. It
        // was selected when it was picked, before this device was, so what is left to do is what
        // picking it now would add: the device remembers the preset it is used with — a
        // microphone its voice preset, as a speaker its `.fac`.
        let picked = picked.and_then(|preset| {
            let at = self.state.presets.iter().position(|e| e.name == preset)?;
            Some((at, preset))
        });
        if let Some((at, preset)) = picked {
            if self.state.selected_preset != Some(at) {
                self.select_preset(at);
            } else {
                self.remember_preset_for_selected_device(&preset);
            }
        } else if let Some(preset) = remembered
            && self
                .state
                .preset()
                .is_none_or(|current| current.name != preset)
            && let Some(at) = self.state.presets.iter().position(|e| e.name == preset)
        {
            self.select_preset(at);
        }

        // With the power off, a pick of the device the lane is already on has nothing to wait
        // for, and the combo goes back to showing where the sound goes: the system's default (see
        // [`lane_selection`]). Only now, so that the preset above was remembered against the pick.
        if !self.state.power {
            self.show_lane_selections();
        }

        // `"Output: "` + name, and the preset in use with it (`FxController.cpp:1150`).
        let preset = self.state.preset().map(|p| p.name.clone());
        self.notify(Message::output_selected(&description, preset.as_deref()));
    }

    /// Detach a lane: hand its default back, stop processing it, remember that it is off.
    ///
    /// The engine tears the lane's nodes down and answers with `Attached { None }`; the other lane
    /// is not touched, whichever of the two the window is editing. With both lanes off FxSound
    /// processes nothing, which is what the two `Off` rows say.
    fn detach_lane(&mut self, direction: DeviceDirection) {
        self.state.set_selection(direction, None);
        self.requested_device[lane_index(direction)] = None;
        set_lane_active(&mut self.state, direction, false);
        self.note_lane_devices();
        self.note_audio(direction);
        self.settings.set_lane_enabled(direction, false);
        self.settings_dirty = true;
        if let Some(engine) = &self.engine {
            engine.send(UiToAudio::DetachLane(direction));
        }
        // Picking the same device again, or the saved one reappearing once the lane is back on,
        // has to reach the engine again.
        self.announced_device[lane_index(direction)] = None;
    }

    /// Make `direction` the lane the window edits. Returns whether it changed.
    ///
    /// Nothing the engine runs changes: this swaps what the window *shows*. The lane being left
    /// goes to its slot of [`App::lane_controls`] as it is, unsaved edits and all; the lane being
    /// entered comes back from its slot as it was left, or — the first time this session — with the
    /// preset the settings file remembers for it, loaded the way start-up loads one, without a
    /// toast.
    fn set_edit_direction(&mut self, direction: DeviceDirection) -> bool {
        if direction == self.state.direction {
            if self.settings.device_direction != direction {
                self.settings.set_edit_direction(direction);
                self.settings_dirty = true;
            }
            return false;
        }
        self.settings.set_edit_direction(direction);
        self.settings_dirty = true;
        self.show_lane(direction);
        self.note_direction();
        true
    }

    /// Put `direction`'s controls in the window and park the other lane's — the part of switching
    /// the edit direction the window sees, without recording the switch. [`App::in_lane`] uses it
    /// alone to act on the lane off screen and come back.
    fn show_lane(&mut self, direction: DeviceDirection) {
        if direction == self.state.direction {
            return;
        }
        // Stored before the other lane is entered, so that whatever entering it asks about the lane
        // being left finds that lane's own controls.
        self.lane_controls[lane_index(self.state.direction)] =
            Some(LaneControls::take(&self.state));
        self.state.direction = direction;

        match self.lane_controls[lane_index(direction)].take() {
            Some(controls) => {
                let selected = controls
                    .selected_preset
                    .and_then(|i| controls.presets.get(i))
                    .map(|p| (p.name.clone(), p.modified));
                controls.put(&mut self.state);
                // The list may have changed while the lane was off screen — an import, a rename —
                // so it is rebuilt, and the selection found again by name with its unsaved-changes
                // marker.
                self.refresh_preset_list();
                self.state.selected_preset = None;
                if let Some((name, modified)) = selected
                    && let Some(at) = self.state.presets.iter().position(|p| p.name == name)
                {
                    self.state.selected_preset = Some(at);
                    self.state.presets[at].modified |= modified;
                }
                self.sync_params_from_state();
                self.note_presets();
            }
            None => self.enter_lane_for_the_first_time(direction),
        }
    }

    /// Do `act` with `lane`'s controls in the window, whichever lane the window is editing, and
    /// leave the window as it was: for what acts on both lanes' presets at once, such as Reset
    /// Presets in Settings, where the lane off screen must change in the engine now rather than
    /// the next time it is looked at. The edit direction is not touched, and nothing `act` says
    /// about the lane off screen reaches the desktop.
    fn in_lane(&mut self, lane: DeviceDirection, act: impl FnOnce(&mut Self)) {
        let edit = self.state.direction;
        if lane == edit {
            act(self);
            return;
        }
        let armed = std::mem::replace(&mut self.notifications_armed, false);
        self.show_lane(lane);
        act(self);
        self.show_lane(edit);
        self.notifications_armed = armed;
    }

    /// Show a lane the window has not edited yet this session.
    fn enter_lane_for_the_first_time(&mut self, direction: DeviceDirection) {
        // Start from what that lane's chain is running, so that a lane with no preset to load
        // shows its own numbers rather than the other lane's.
        match direction {
            DeviceDirection::Output => {
                self.state.filter_q = self.settings.filter_q;
                self.state.master_gain_db = self.settings.master_gain;
                self.state.balance_db = self.settings.balance;
                self.state.volume_leveling = self.settings.volume_leveling;
                for effect in Effect::ALL {
                    self.state.effects[effect as usize] =
                        scale::value_to_slider_for(effect, self.params.effect(effect));
                }
                self.state.eq_on = self.params.eq_on;
                let (centres, boosts) = self.params.bands();
                self.state.eq_bands = bands_of(centres, boosts);
                // On the user's band count even with no preset to load (U1): the snapshot starts
                // on ten bands whatever the settings file says.
                let ladder = self.music_ladder();
                if ladder.len() != self.state.eq_bands.len() {
                    let gains = fxsound_dsp::eq::remap_band_gains(boosts, ladder.len());
                    self.state.eq_bands = bands_of(&ladder, &gains);
                }
            }
            DeviceDirection::Input => {
                let params = self.input_params;
                self.state.eq_on = params.eq_on;
                let (centres, boosts) = params.bands();
                self.state.eq_bands = bands_of(centres, boosts);
                self.state.filter_q = params.filter_q;
                self.state.master_gain_db = params.makeup_db;
                self.state.denoise_on = params.rnnoise;
                self.state.gate_on = params.gate_on;
                self.state.compressor_on = params.compressor_on;
                self.state.deesser_on = params.deesser_on;
            }
        }
        self.refresh_preset_list();
        self.state.selected_preset = None;
        // By lane rather than through the edit direction: start-up loads the lane off screen
        // through here too, before the edit direction has been entered.
        let saved = self.settings.preset_for_direction(direction).to_owned();
        let index = self
            .state
            .presets
            .iter()
            .position(|p| p.name == saved)
            .or_else(|| (!self.state.presets.is_empty()).then_some(0));
        if let Some(index) = index {
            // Not something to announce: the user asked to look at a lane, not for a preset.
            let armed = std::mem::replace(&mut self.notifications_armed, false);
            self.select_preset(index);
            self.notifications_armed = armed;
        }
        // Published whether or not a preset was loaded: with none to load, or one that failed to,
        // the lane runs what it was seeded with above rather than what its snapshot last held.
        self.sync_params_from_state();
        self.note_presets();
    }

    /// The same controls, mapped onto the microphone chain.
    ///
    /// Both snapshots are published every time, whichever lanes the engine is running: they are
    /// state, so the audio thread always reads a current one and attaching a lane needs no
    /// handshake.
    ///
    /// **Who owns what.** Three controls mean the same thing on a voice as on music and are
    /// carried straight across: the power switch, the ten-band equalizer — the one thing the two
    /// chains genuinely share — and the output gain, which becomes the chain's makeup. The four
    /// stage switches come from [`UiState`] too, and a voice preset is what moves them.
    ///
    /// Everything else — the high-pass corner and order, and the gate, compressor and de-esser
    /// numbers — belongs to the **preset** and is deliberately not touched here. There is no
    /// control for any of it, which is the point: the voice set is navigated by preset, so a
    /// mapping that overwrote a preset's high-pass with a constant would undo the thing the preset
    /// was for. It did, until a test caught it.
    ///
    /// With no preset selected the chain runs on `InputDspParams::default()` — an 80 Hz
    /// second-order high-pass and every dynamics stage off. The high-pass is the only stage right
    /// for every microphone regardless of voicing; nothing else should start working on someone's
    /// voice before they have chosen it.
    ///
    /// What this mapping does *not* carry: the five effect sliders, the balance and the volume
    /// leveller, none of which has a counterpart in a voice chain. They are inert while a
    /// microphone is selected, and the interface says so.
    fn sync_input_params_from_state(&mut self) {
        self.input_params.eq_on = self.state.eq_on;
        self.input_params.set_bands(&self.state.eq_bands);
        self.input_params.filter_q = self.state.filter_q;
        self.input_params.makeup_db = self.state.master_gain_db;

        self.input_params.rnnoise = self.state.denoise_on;
        self.input_params.gate_on = self.state.gate_on;
        self.input_params.compressor_on = self.state.compressor_on;
        self.input_params.deesser_on = self.state.deesser_on;
    }

    /// Act on the `--output` and `--input` that arrived before the device list did.
    ///
    /// Taken rather than retried: a name that is not in the list *now that there is one* is a name
    /// that is not a device, and holding it for the next list would mean a typo at login quietly
    /// changing the device half a minute later when something unrelated is plugged in. A name is
    /// resolved exactly as it would have been had the list been there when it arrived
    /// ([`crate::commands::resolve_device`]), 0.3.0's `--output` naming a microphone included —
    /// and a preset picked for the lane in the meantime is kept rather than replaced by the one
    /// the device remembers, since with the list there it would have been picked after the device.
    /// The same goes for the edit direction: holding a name made its lane the edit direction, and
    /// if the window is on the other lane by now, something later — `--edit`, the window, a device
    /// that was listed — chose that, and it stays chosen.
    pub(crate) fn apply_pending_device(&mut self) {
        use crate::commands::{Resolved, resolve_device};
        let pending = std::mem::take(&mut self.pending_devices);
        let settled_edit = pending
            .last()
            .is_some_and(|last| last.lane != self.state.direction)
            .then_some(self.state.direction);
        for PendingDevice {
            lane,
            name: wanted,
            preset,
        } in pending
        {
            // The invoking process has long since exited, so there is nobody left to return a
            // status to. The log is the only place anything here can be said.
            match resolve_device(&self.state.devices, &wanted, lane) {
                Resolved::Found(index) => self.select_device_keeping(index, lane, preset),
                // A preset picked for the output lane means nothing on the microphone this turned
                // out to be, so the device brings back its own.
                Resolved::OtherDirection(index) if lane == DeviceDirection::Output => {
                    log::info!("{wanted:?} is a microphone; --output selected it as --input would");
                    self.select_device(index, DeviceDirection::Input);
                }
                Resolved::OtherDirection(_) => {
                    log::warn!(
                        "{wanted:?} is a playback device, not a microphone; --input did nothing"
                    );
                }
                Resolved::NotFound => log::warn!(
                    "no audio device is called {wanted:?}; --{} did nothing",
                    lane.key()
                ),
            }
        }
        if let Some(edit) = settled_edit {
            self.set_edit_direction(edit);
        }
        self.save_settings_if_dirty();
    }

    /// What the audio thread last said about the lane the window is editing — the negotiated
    /// format, and how the ring between that lane's two nodes is coping.
    #[must_use]
    pub const fn audio_status(&self) -> &fxsound_core::AudioStatus {
        self.audio_status_for(self.state.direction)
    }

    /// What the audio thread last said about one lane.
    #[must_use]
    pub const fn audio_status_for(&self, direction: DeviceDirection) -> &fxsound_core::AudioStatus {
        &self.audio_status[lane_index(direction)]
    }

    /// The `node.name` a lane is attached to, as the engine last reported it; `None` while the
    /// lane has no nodes — detached, reconnecting, or not attached yet.
    #[must_use]
    pub fn attached(&self, direction: DeviceDirection) -> Option<&str> {
        self.attached[lane_index(direction)].as_deref()
    }

    /// Whether a device list has ever arrived.
    #[must_use]
    pub const fn has_seen_devices(&self) -> bool {
        self.devices_seen
    }

    /// Select this device for `lane` as soon as a device list exists.
    ///
    /// Used by `--output` and `--input` when they run before enumeration has finished, which is
    /// what anything started at login does. One pending name per lane, not a queue: a second
    /// `--output` supersedes the first exactly as a second one would if both had arrived after the
    /// list, and leaves a pending `--input` alone. So does anything else that settles the lane's
    /// device in the meantime — `off`, a device that is in the list — through
    /// [`App::cancel_pending_device`].
    ///
    /// A preset picked for `lane` while the name waits — the `--preset` on the same line, a later
    /// one, the window's picker — is held with it, and the device keeps that preset when it is
    /// selected instead of bringing back the one it remembers.
    pub fn select_device_when_listed(&mut self, name: &str, lane: DeviceDirection) {
        self.cancel_pending_device(lane);
        self.pending_devices.push(PendingDevice {
            lane,
            name: name.to_owned(),
            preset: None,
        });
    }

    /// Forget the device `lane` was waiting for, so the list's arrival does not select it after
    /// something later on the command line — `off`, `--next-*`, a device already listed — has
    /// settled what the lane should be.
    pub fn cancel_pending_device(&mut self, lane: DeviceDirection) {
        self.pending_devices.retain(|pending| pending.lane != lane);
    }

    /// Whether `lane` is waiting for the device list to select a device, and which name.
    #[must_use]
    pub fn pending_device(&self, lane: DeviceDirection) -> Option<&str> {
        self.pending_devices
            .iter()
            .find(|pending| pending.lane == lane)
            .map(|pending| pending.name.as_str())
    }

    /// After an explicit preset pick: if the edit direction's device is still waiting for the
    /// list, the pick goes with it (see [`PendingDevice::preset`]).
    fn hold_picked_preset(&mut self) {
        let lane = self.state.direction;
        let picked = self.state.preset().map(|p| p.name.clone());
        if let Some(pending) = self
            .pending_devices
            .iter_mut()
            .find(|pending| pending.lane == lane)
        {
            pending.preset = picked;
        }
    }

    /// The preset a lane has selected — its name and whether it carries unsaved changes —
    /// whichever lane the window is editing. For `--status`, which reports both.
    ///
    /// The edit direction's is what the window shows; the other lane's is in that lane's own
    /// stored controls, as the window last left it. Only an app that has never shown the lane —
    /// one built by [`App::headless_for_tests`], since start-up enters both — has none to read,
    /// and answers with the name the settings file remembers for it.
    #[must_use]
    pub fn lane_preset(&self, lane: DeviceDirection) -> Option<(&str, bool)> {
        if lane == self.state.direction {
            return self.state.preset().map(|p| (p.name.as_str(), p.modified));
        }
        if let Some(controls) = &self.lane_controls[lane_index(lane)] {
            return controls
                .selected_preset
                .and_then(|i| controls.presets.get(i))
                .map(|p| (p.name.as_str(), p.modified));
        }
        let saved = self.settings.preset_for_direction(lane);
        (!saved.is_empty()).then_some((saved, false))
    }

    /// `lane`'s preset list as its picker lists it — factory presets first, each with its
    /// unsaved-changes marker — whichever lane the window is editing. For `--status`, which lists
    /// both (upstream's `printStatus` lists the one it has, `FxController.cpp:616-636`).
    ///
    /// The edit direction's is the window's own list. The other lane's is read from its store, as
    /// the window would list it on switching over, with the unsaved changes of the selected
    /// preset that the lane's stored controls still hold.
    #[must_use]
    pub fn lane_preset_list(&self, lane: DeviceDirection) -> Vec<PresetEntry> {
        if lane == self.state.direction {
            return self.state.presets.clone();
        }
        let selected = self.lane_preset(lane);
        self.store(lane)
            .entries()
            .iter()
            .map(|entry| PresetEntry {
                name: entry.name.clone(),
                factory: entry.source == fxsound_preset::PresetSource::Factory,
                modified: entry.modified
                    || selected.is_some_and(|(name, modified)| modified && name == entry.name),
            })
            .collect()
    }

    /// Whether `lane`'s preset list has one called `name` — the `.fac` store for the speakers,
    /// the voice store, the user's own voice presets included, for the microphone — whichever
    /// lane the window is showing.
    #[must_use]
    pub fn lane_has_preset(&self, lane: DeviceDirection, name: &str) -> bool {
        self.store(lane).entries().iter().any(|e| e.name == name)
    }

    /// Pin the microphone's noise suppression, or hand it back to the voice preset — the Settings
    /// pane's choice, from the command line. Saved and published at once, whichever lane the
    /// window is editing; an open pane picks it up in [`App::refresh_settings_state`].
    pub fn set_noise_suppression(&mut self, choice: fxsound_core::NoiseSuppressionOverride) {
        if self.settings.noise_suppression == choice {
            return;
        }
        self.settings.noise_suppression = choice;
        self.microphone_setting_changed();
    }

    /// Pretend a device list has arrived, so a name that is not in `state.devices` is refused
    /// rather than held — the command-line tests' way past the login race.
    #[doc(hidden)]
    pub fn mark_devices_seen_for_tests(&mut self) {
        self.devices_seen = true;
    }

    /// Point a headless app at these preset directories and these voice presets, and show the
    /// edit direction's list — the command-line tests' way to a preset list without the user's.
    ///
    /// The voices are written as the factory set into `VoiceFactory/` under `user_dir`, and the
    /// user's voice presets go to `Input/` under it, the layout the real stores use.
    #[doc(hidden)]
    pub fn use_presets_for_tests(
        &mut self,
        factory_dirs: Vec<PathBuf>,
        user_dir: PathBuf,
        voices: Vec<InputPreset>,
    ) {
        self.presets = PresetStore::with_dirs(factory_dirs, user_dir.clone());
        self.presets.rescan();
        self.voice_presets = voice_store_for_tests(
            &voices,
            &user_dir.join("VoiceFactory"),
            user_dir.join("Input"),
        );
        self.refresh_preset_list();
        self.state.selected_preset = None;
        self.note_presets();
    }

    /// The microphone snapshot currently published, for tests and for `--status`.
    #[must_use]
    pub const fn input_params(&self) -> &InputDspParams {
        &self.input_params
    }

    /// The voice chain the audio thread was last told to run, by the name the preset gave it.
    #[must_use]
    pub fn input_chain(&self) -> &str {
        &self.input_chain
    }

    /// The snapshot currently published, for tests and for the CLI's `--status`.
    #[must_use]
    pub const fn params(&self) -> &DspParams {
        &self.params
    }

    /// An app with no audio engine and no preset directory — the shape the controller tests and
    /// the command-line tests both need.
    ///
    /// Kept out of `cfg(test)` so sibling modules can use it; it is harmless in a release build
    /// and costs one unused function.
    #[doc(hidden)]
    #[must_use]
    pub fn headless_for_tests() -> Self {
        let mut app = Self {
            state: UiState::default(),
            params: DspParams::default(),
            input_params: unvoiced_input_params(),
            input_voicing: PresetVoicing::of(&unvoiced_input_params()),
            echo_cancel_detail: String::new(),
            input_chain: fxsound_preset::input::DEFAULT_CHAIN.to_owned(),
            voice_presets: InputPresetStore::with_dirs(
                Vec::new(),
                std::env::temp_dir().join("fxsound-app-test").join("Input"),
            ),
            devices_seen: false,
            audio_status: [fxsound_core::AudioStatus::default(); 2],
            attached: [None, None],
            requested_device: [None, None],
            preset_device: [None, None],
            volume_save_due: None,
            pending_devices: Vec::new(),
            settings: Settings::default(),
            presets: PresetStore::with_dirs(
                Vec::new(),
                std::env::temp_dir().join("fxsound-app-test"),
            ),
            loaded_preset: None,
            loaded_voice: None,
            engine: None,
            assets: AssetCache::new(),
            settings_dirty: false,
            persist: false,
            export_dir: std::env::temp_dir().join("fxsound-app-test-export"),
            announced_device: [None, None],
            notifier: Notifier::new(true),
            notifications_armed: true,
            tray_tip_shown: false,
            lane_controls: [None, None],
            events: Vec::new(),
            published: Published::default(),
            tray_stale: false,
            calibration: None,
            priority_sent: [None, None],
            sleeping: false,
            shown_meters: Meters::default(),
            meters_moved: false,
        };
        app.start_the_stream_here();
        app
    }

    /// logind's `PrepareForSleep` (U13, [`crate::sleep`]): `true` as the system goes to sleep,
    /// `false` once it has resumed.
    ///
    /// Going to sleep, both lanes are muted — silence after each chain, carried in the snapshots —
    /// so that the last buffers before the suspend never reach the speakers as a burst, and the
    /// engine is told, to wait out the devices coming back rather than choose among them as they
    /// do. Resumed, the engine is told first, then each lane's filters are cleared of what they
    /// held when the machine stopped, and only then are the lanes unmuted: the events reach the
    /// audio thread no later than the snapshot, so the first sound after the resume is the chain
    /// starting clean, not the tail of what was playing before. Upstream mutes and unmutes the
    /// same way (`FxController.cpp:2145-2159`) and, as here, never holds the suspend up with an
    /// inhibitor (its PR #533).
    ///
    /// The same word twice is said once: logind repeats nothing, but a watcher whose connection
    /// drops while the system sleeps says `false` for it ([`crate::sleep::SleepWatch`]).
    pub fn system_sleeping(&mut self, sleeping: bool) {
        if sleeping == self.sleeping {
            return;
        }
        self.sleeping = sleeping;
        log::info!(
            "{}",
            if sleeping {
                "the system is going to sleep: muting both lanes"
            } else {
                "the system has resumed: clearing both lanes' filters and unmuting"
            }
        );
        self.send(UiToAudio::SystemSleeping(sleeping));
        if !sleeping && let Some(engine) = &self.engine {
            for direction in DeviceDirection::ALL {
                engine.send_event(direction, DspEvent::ResetFilterState);
            }
        }
        self.sync_params_from_state();
    }

    /// Whether the system is asleep as far as the controller knows ([`App::system_sleeping`]).
    #[must_use]
    pub const fn is_system_sleeping(&self) -> bool {
        self.sleeping
    }

    /// Persist settings and stash unsaved preset edits. Called on the way out.
    ///
    /// Each lane's unsaved edits are stashed whichever lane the window is editing, read from that
    /// lane's own controls and filed in that lane's own store — the speakers' as a `.fac` autosave,
    /// the microphone's as a voice one under `Input/AutoSave/` — so a restart brings them back as a
    /// preset switch would.
    pub fn shutdown(&mut self) {
        // A wizard left open lets go of the microphone before the engine goes.
        self.cancel_calibration();
        for lane in DeviceDirection::ALL {
            if let Some(preset) = self.unsaved_lane_preset(lane) {
                self.autosave_lane_preset(&preset);
            }
        }
        if self.persist
            && let Err(err) = self.settings.save()
        {
            log::warn!("could not save settings on exit: {err}");
        }
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
        }
    }
}

// =============================================================================================
// Events: what changed, said where it changed (0.4.0 design §10)
// =============================================================================================

impl App {
    /// Everything that changed since the last call, in the order it changed. The runtime drains
    /// this once a tick and hands the same events to the `--watch` streams, the D-Bus service and
    /// the tray ([`crate::events::fan_out`]).
    pub fn drain_events(&mut self) -> Vec<AppEvent> {
        std::mem::take(&mut self.events)
    }

    /// Whether the tray has to be redrawn for something no event names — the theme, Always On
    /// Top, the language, a preset list that changed under its selection, sound starting or
    /// stopping on the shown lane — and forget it. An event that touches the tray
    /// ([`AppEvent::touches_tray`]) is the other reason.
    pub fn take_tray_refresh(&mut self) -> bool {
        std::mem::take(&mut self.tray_stale)
    }

    /// Put up a notice in the window and tell the stream: the one way the application raises
    /// one. The same text raised twice is two notices, each with its own four seconds.
    pub fn raise_notice(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.state.notify(text.clone());
        self.events.push(AppEvent::Notice { message: text });
    }

    /// Keep what the calibration wizard applied — the voice preset it wrote, the microphone and
    /// what it measured there — in the settings file, and say so (0.4.0 design §8). A record
    /// with a measurement that is not a number is not one, and changes nothing.
    pub fn record_calibration(&mut self, record: CalibrationRecord) {
        let measured = [
            record.noise_floor_db,
            record.speech_rms_db,
            record.speech_peak_db,
            record.clipped_ratio,
        ];
        if !measured.iter().all(|value| value.is_finite()) {
            log::warn!("a calibration that measured something other than a number was dropped");
            return;
        }
        let record = CalibrationRecord {
            clipped_ratio: record.clipped_ratio.clamp(0.0, 1.0),
            ..record
        };
        self.settings.calibration = Some(record.clone());
        self.persist_settings();
        self.events.push(AppEvent::Calibrated(record));
    }

    /// What a lane's audio is doing, as `audio_state` reports it: nothing at all without an
    /// engine, processing while its last status said so, idle otherwise.
    #[must_use]
    pub const fn lane_state(&self, direction: DeviceDirection) -> LaneState {
        if !self.has_audio() {
            return LaneState::Unavailable;
        }
        let active = match direction {
            DeviceDirection::Output => self.state.output_active,
            DeviceDirection::Input => self.state.input_active,
        };
        if active {
            LaneState::Processing
        } else {
            LaneState::Idle
        }
    }

    /// The audio thread's reason the echo canceller is not running, or empty.
    #[must_use]
    pub fn echo_cancel_detail(&self) -> &str {
        &self.echo_cancel_detail
    }

    /// Changes the stream has not been told about — empty whenever every mutation said what it
    /// changed. For the tests, which run it after every step as their oracle.
    #[doc(hidden)]
    #[must_use]
    pub fn unsaid_changes(&self) -> Vec<AppEvent> {
        self.published.unsaid(self)
    }

    /// Make the controller as it is now what the stream starts from, with nothing queued.
    fn start_the_stream_here(&mut self) {
        self.published = Published::of(self);
        self.events.clear();
        self.tray_stale = false;
    }

    /// Hold the thing `check` reads against what the stream last said about it, and queue the
    /// event that says it changed, if it did.
    fn note(&mut self, check: impl FnOnce(&mut Published, &Self) -> Option<AppEvent>) {
        // Out of `self` for the call, so that the check can read the whole controller.
        let mut published = std::mem::take(&mut self.published);
        let event = check(&mut published, self);
        self.published = published;
        self.events.extend(event);
    }

    fn note_power(&mut self) {
        self.note(|published, app| published.power(app.state.power));
    }

    fn note_direction(&mut self) {
        self.note(|published, app| published.direction(app.state.direction));
    }

    /// Both lanes' presets: wherever a list, a selection or an unsaved-changes marker moves.
    /// Both, because acting on the lane off screen (Reset Presets) goes through the same paths.
    fn note_presets(&mut self) {
        for direction in DeviceDirection::ALL {
            self.note(|published, app| published.preset(direction, app.lane_preset(direction)));
        }
    }

    /// Both lanes' devices: wherever a selection is set or the list under it changes.
    fn note_lane_devices(&mut self) {
        for direction in DeviceDirection::ALL {
            self.note(|published, app| {
                published.device(direction, app.state.device_for(direction))
            });
        }
    }

    fn note_device_list(&mut self) {
        self.note(|published, app| published.devices(&app.state.devices));
    }

    fn note_audio(&mut self, direction: DeviceDirection) {
        self.note(|published, app| {
            published.audio(
                direction,
                app.lane_state(direction),
                app.audio_status_for(direction),
            )
        });
    }

    fn note_echo_cancel(&mut self) {
        self.note(|published, app| {
            published.echo_cancel(
                app.state.echo_cancel_on,
                app.state.echo_cancel_running,
                &app.echo_cancel_detail,
            )
        });
    }
}

/// Whether a window exists right now.
///
/// On Wayland a client cannot unmap and later remap its toplevel through winit
/// (`winit-0.30.13/src/platform_impl/linux/wayland/window/mod.rs:253`, `set_visible` is a no-op),
/// so "hidden to the tray" means *no window at all*: the shell destroys it and keeps the engine,
/// the tray and the control socket running headless until something asks for the window back.
/// This is the shell's record of which of the two states it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowVisibility {
    Shown,
    Hidden,
}

impl App {
    /// The saved "start minimised" preference.
    #[must_use]
    pub const fn settings_run_minimized(&self) -> bool {
        self.settings.run_minimized
    }

    /// A fresh mirror of the model for the tray to draw its menu and tooltip from: both lanes,
    /// each with its presets and its device (0.4.0 design §1.4).
    #[must_use]
    pub fn tray_state(&self) -> crate::tray::TrayState {
        crate::tray::TrayState {
            power: self.state.power,
            // The shown lane's non-silent buffers, as the window's logo, the visualizer and
            // `--status`'s `audio` read them: the original's one `audio_process_on_` drives all
            // of them (`docs/spec/05-controller-model.md`). Not the lane's `audio_state`, which
            // stays `processing` while a paused stream keeps the device running on silence.
            processing: self.state.audio_active,
            power_enabled: true,
            theme: self.state.theme,
            output: self.tray_lane(DeviceDirection::Output),
            input: self.tray_lane(DeviceDirection::Input),
            devices: self
                .state
                .devices
                .iter()
                .map(|d| crate::tray::TrayDevice {
                    name: d.description.clone(),
                    direction: d.direction,
                })
                .collect(),
            language: i18n::current(),
        }
    }

    /// One lane as the tray draws it: the lane's presets as its picker lists them, whichever lane
    /// the window is editing, and its device — an index into the same list the tray's devices are.
    fn tray_lane(&self, lane: DeviceDirection) -> crate::tray::TrayLane {
        let presets = self.lane_preset_list(lane);
        let selected_preset = if lane == self.state.direction {
            self.state.selected_preset
        } else {
            self.lane_preset(lane)
                .and_then(|(name, _)| presets.iter().position(|p| p.name == name))
        };
        crate::tray::TrayLane {
            presets: presets
                .into_iter()
                .map(|p| crate::tray::TrayPreset {
                    name: p.name,
                    factory: p.factory,
                    modified: p.modified,
                })
                .collect(),
            selected_preset,
            device: self.state.selection(lane),
        }
    }

    /// Act on a tray menu choice.
    ///
    /// The window-level items (`Open`, `ToggleWindow`, `Exit`) never reach here — the shell deals
    /// with those, because only it owns a viewport.
    pub fn handle_tray(&mut self, command: crate::tray::TrayCommand) {
        use crate::tray::TrayCommand;
        match command {
            TrayCommand::SetPower(on) => {
                if on != self.state.power {
                    self.handle(&[UiAction::TogglePower]);
                }
            }
            TrayCommand::SelectPreset { direction, name } => {
                self.select_lane_preset(direction, &name);
            }
            // A pick in the tray is the later word on its lane, as a later command line's is: a
            // `--output` still waiting for the device list must not take the lane back when the
            // list comes.
            TrayCommand::SelectDevice(index) => {
                if let Some(direction) = self.state.devices.get(index).map(|d| d.direction) {
                    self.cancel_pending_device(direction);
                }
                self.handle(&[UiAction::SelectDevice(index)]);
            }
            TrayCommand::Detach(direction) => {
                self.cancel_pending_device(direction);
                self.handle(&[UiAction::detach(direction)]);
            }
            TrayCommand::SetTheme(theme) => {
                if theme != self.state.theme {
                    self.handle(&[UiAction::ToggleTheme]);
                }
            }
            TrayCommand::OpenSettings => self.handle(&[UiAction::OpenSettings]),
            // Handled by the shell.
            TrayCommand::ToggleWindow | TrayCommand::Open | TrayCommand::Exit => {}
        }
    }

    /// Select `lane`'s preset called `name`, whichever lane the window is editing, and leave the
    /// window on the lane it was showing — the tray's two preset submenus. Picked as the window's
    /// list picks one, toast and all; a name the lane does not have (the list changed after the
    /// menu was drawn) does nothing.
    fn select_lane_preset(&mut self, lane: DeviceDirection, name: &str) {
        let edit = self.state.direction;
        self.show_lane(lane);
        if let Some(at) = self.state.presets.iter().position(|p| p.name == name) {
            self.handle(&[UiAction::SelectPreset(at)]);
        }
        self.show_lane(edit);
    }
}

impl App {
    /// `FxController::getMaxUserPresets()`: the setting, with anything below 10 or above 120 read
    /// as 120 (`FxController.cpp:194-198`, `docs/spec/03-controls.md` §8.5).
    #[must_use]
    pub fn max_user_presets(&self) -> usize {
        let max = self.settings.max_user_presets;
        if (10..=120).contains(&max) {
            max as usize
        } else {
            120
        }
    }

    /// Whether `command` may run on the edit direction's presets now, and if not, why: the one
    /// rule behind the hamburger menu's items ([`App::preset_menu`]) and the preset options of the
    /// command line, the control socket and D-Bus (`commands::run`).
    ///
    /// The menu's own predicates (`FxMainWindow.cpp:536-543`) and the original's command line
    /// (`FxController.cpp:377-448`) agree on all of it but the power switch: the menu greys every
    /// preset item out while the power is off and the original's command line ignores them, but a
    /// script here gets its preset command carried out either way. A new or renamed preset's name
    /// is checked last, so a refusal about the name ([`Refusal::is_about_the_name`]) means
    /// everything else would allow it.
    ///
    /// # Errors
    ///
    /// The [`Refusal`] that stands in the way.
    pub fn preset_command_allowed(&self, command: &PresetCommand) -> Result<(), Refusal> {
        let selected = || self.state.preset().ok_or(Refusal::NoPresetSelected);
        match command {
            PresetCommand::Select(name) => {
                if self.state.presets.iter().any(|p| p.name == *name) {
                    return Ok(());
                }
                let lane = self.state.direction;
                Err(Refusal::UnknownPreset {
                    lane,
                    name: name.clone(),
                    other_lane_has_it: self.lane_has_preset(lane.other(), name),
                })
            }
            PresetCommand::SaveAs(name) => {
                let preset = selected()?;
                if !preset.modified {
                    return Err(Refusal::NothingToSave {
                        preset: preset.name.clone(),
                    });
                }
                let max = self.max_user_presets();
                if self.user_preset_count() >= max {
                    return Err(Refusal::LimitReached { max });
                }
                self.new_name_allowed(name)
            }
            PresetCommand::Overwrite => {
                let preset = selected()?;
                if preset.factory {
                    return Err(Refusal::FactoryOverwrite {
                        preset: preset.name.clone(),
                    });
                }
                if !preset.modified {
                    return Err(Refusal::NothingToSave {
                        preset: preset.name.clone(),
                    });
                }
                Ok(())
            }
            PresetCommand::Undo => {
                let preset = selected()?;
                if preset.modified {
                    Ok(())
                } else {
                    Err(Refusal::NothingToUndo {
                        preset: preset.name.clone(),
                    })
                }
            }
            PresetCommand::Rename(name) => {
                let preset = selected()?;
                if preset.factory {
                    return Err(Refusal::FactoryRename {
                        preset: preset.name.clone(),
                    });
                }
                if preset.modified {
                    return Err(Refusal::UnsavedChanges {
                        preset: preset.name.clone(),
                    });
                }
                self.new_name_allowed(name)
            }
            PresetCommand::Delete => {
                let preset = selected()?;
                if preset.factory {
                    Err(Refusal::FactoryDelete {
                        preset: preset.name.clone(),
                    })
                } else {
                    Ok(())
                }
            }
            PresetCommand::Next | PresetCommand::Previous => Ok(()),
        }
    }

    /// Whether a new or renamed preset may be called `name` ([`preset_name_available`]).
    fn new_name_allowed(&self, name: &str) -> Result<(), Refusal> {
        let name = name.trim();
        if name.is_empty() {
            Err(Refusal::EmptyName)
        } else if self.is_preset_name_available(name) {
            Ok(())
        } else {
            Err(Refusal::NameTaken {
                name: name.to_owned(),
            })
        }
    }

    /// The hamburger menu's preset items: each offered when [`App::preset_command_allowed`]
    /// allows its command and the power is on — Save New Preset and Rename Preset whatever the
    /// name, which their editor asks for.
    #[must_use]
    pub fn preset_menu(&self) -> PresetMenu {
        let offered = |command: PresetCommand| {
            self.state.power
                && self
                    .preset_command_allowed(&command)
                    .or_else(|refusal| {
                        if refusal.is_about_the_name() {
                            Ok(())
                        } else {
                            Err(refusal)
                        }
                    })
                    .is_ok()
        };
        PresetMenu {
            save_new: offered(PresetCommand::SaveAs(String::new())),
            overwrite: offered(PresetCommand::Overwrite),
            undo: offered(PresetCommand::Undo),
            rename: offered(PresetCommand::Rename(String::new())),
            delete: offered(PresetCommand::Delete),
        }
    }

    /// `FxModel::getUserPresetCount()`.
    #[must_use]
    pub fn user_preset_count(&self) -> usize {
        self.state.presets.iter().filter(|p| !p.factory).count()
    }

    /// Whether `name` can be given to a new or renamed preset ([`preset_name_available`]).
    #[must_use]
    pub fn is_preset_name_available(&self, name: &str) -> bool {
        preset_name_available(&self.state.presets, name)
    }

    /// `FxController::renamePreset` — the menu's Rename Preset item, on the edit direction's
    /// store.
    ///
    /// User presets only; a factory preset refuses with a notification, as deleting one does.
    /// Implemented as save-under-the-new-name then delete-the-old, which is what the original does
    /// through its preset list rather than a filesystem rename. The saved file is what gets
    /// renamed, not unsaved edits: the menu only offers Rename while the preset is unmodified
    /// (`docs/spec/03-controls.md` §8.5), and a caller that ignores that keeps its edits in the
    /// autosave under the *old* name, which the store's delete then removes.
    ///
    /// Everything that named the preset follows it: the lane's saved preset and every device that
    /// remembers it, so plugging one back in does not look for a name that no longer exists.
    pub fn rename_preset(&mut self, new_name: &str) {
        let new_name = new_name.trim();
        let Some(entry) = self.state.preset() else {
            return;
        };
        if entry.factory {
            self.raise_notice(tr("Factory presets cannot be renamed"));
            return;
        }
        let old = entry.name.clone();
        if new_name.is_empty() || new_name == old {
            return;
        }
        if !self.is_preset_name_available(new_name) {
            self.raise_notice(tr_args("A preset named %s already exists", &[new_name]));
            return;
        }

        let lane = self.state.direction;
        if let Err(err) = self.store_mut(lane).rename(&old, new_name) {
            log::warn!("could not rename preset {old} to {new_name}: {err}");
            self.raise_notice(tr_args("Could not rename %s", &[old.as_str()]));
            return;
        }

        match lane {
            DeviceDirection::Output => {
                if let Some(preset) = &mut self.loaded_preset {
                    new_name.clone_into(&mut preset.name);
                }
            }
            DeviceDirection::Input => {
                if let Some(preset) = &mut self.loaded_voice {
                    new_name.clone_into(&mut preset.name);
                }
            }
        }
        self.refresh_preset_list();
        self.state.selected_preset = self.state.presets.iter().position(|p| p.name == new_name);
        self.settings.set_preset_for_direction(lane, new_name);
        for config in &mut self.settings.device_configs {
            if config.direction == lane && config.preset == old {
                new_name.clone_into(&mut config.preset);
            }
        }
        self.settings_dirty = true;
        self.note_presets();
        self.raise_notice(tr_args("Renamed %s to %s", &[old.as_str(), new_name]));
        // Direct callers (the menu) do not go through `handle`, so flush here.
        self.handle(&[]);
    }

    /// `FxController::importPresets()` (`FxController.cpp:1419-1458`) over the non-recursive
    /// glob of `FxPresetImportDialog.cpp:255-278`, into the edit direction's store: `*.fac` into
    /// the speakers' presets, `*.toml` voice presets into the microphone's.
    ///
    /// Returns `None` when the folder holds no preset files of that kind at all — the caller shows
    /// `"Preset files not found in the selected folder."` and leaves the window open. Otherwise
    /// every file whose name is already taken (case-insensitively) is skipped and the rest are
    /// copied into the lane's user preset directory.
    pub fn import_presets(&mut self, folder: &Path) -> Option<ImportSummary> {
        let lane = self.state.direction;
        let extension = self.store(lane).extension();
        let mut files: Vec<PathBuf> = match std::fs::read_dir(folder) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && path
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
                })
                .collect(),
            Err(err) => {
                log::warn!("could not read {}: {err}", folder.display());
                return None;
            }
        };
        if files.is_empty() {
            return None;
        }
        files.sort();

        let mut summary = ImportSummary::default();
        for path in files {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map_or_else(|| path.display().to_string(), str::to_owned);
            // `Store::import` names the preset after the file, so that is the name to check.
            if !self.is_preset_name_available(&stem) {
                summary.skipped.push(stem);
                continue;
            }
            match self.store_mut(lane).import(&path) {
                Ok(name) => summary.imported.push(name),
                Err(err) => {
                    // Not a duplicate, but the summary has no third column; the log has the reason.
                    log::warn!("could not import {}: {err}", path.display());
                    summary.skipped.push(stem);
                }
            }
        }

        if !summary.imported.is_empty() {
            self.refresh_preset_list_keeping_selection();
        }
        Some(summary)
    }

    /// Act on one Import-window action. Returns `true` when the window should close.
    ///
    /// [`PresetsAction::ChooseImportFolder`] is not handled here: it starts a native folder
    /// picker, which is the shell's business, and the shell puts the answer in
    /// [`ImportState::folder`].
    pub fn handle_import(&mut self, action: &PresetsAction, state: &mut ImportState) -> bool {
        match action {
            PresetsAction::Import => {
                let Some(folder) = state.folder.clone() else {
                    return false;
                };
                match self.import_presets(&folder) {
                    Some(summary) => state.summary = Some(summary),
                    None => {
                        state.notice =
                            Some(fxsound_ui::dialogs::presets::NO_PRESETS_FOUND.to_owned());
                    }
                }
                false
            }
            PresetsAction::DismissNotice => {
                state.notice = None;
                false
            }
            PresetsAction::CloseImport => true,
            PresetsAction::ChooseImportFolder
            | PresetsAction::ToggleExport(_)
            | PresetsAction::Export
            | PresetsAction::Overwrite(_)
            | PresetsAction::RevealExportFolder
            | PresetsAction::CloseExport => false,
        }
    }

    /// Where Export Presets writes: `Documents\FxSound\Presets\Export` in the original
    /// (`FxController.cpp:1384-1417`), under the XDG documents directory here.
    #[must_use]
    pub fn export_dir(&self) -> &Path {
        &self.export_dir
    }

    /// Act on one Export-window action. Returns `true` when the window should close.
    ///
    /// `FxController::exportPresets()` asks about each colliding file from inside its loop; here
    /// the collisions are computed first and the dialog asks once, which is what
    /// [`ExportState::collisions`] is for (`docs/spec/06-dialogs.md` §3.1, open question 3).
    pub fn handle_export(&mut self, action: &PresetsAction, state: &mut ExportState) -> bool {
        match action {
            PresetsAction::ToggleExport(index) => {
                if !state.exporting && *index < state.presets.len() && !state.selected.remove(index)
                {
                    state.selected.insert(*index);
                }
                false
            }
            PresetsAction::Export => {
                if !state.can_export() {
                    return false;
                }
                state.exporting = true;
                let names: Vec<String> = state
                    .selected_names()
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                let collisions = self.export_collisions(&names);
                if collisions.is_empty() {
                    let written = self.export_presets(&names);
                    state.finished = Some(written > 0);
                } else {
                    state.collisions = collisions;
                }
                false
            }
            PresetsAction::Overwrite(choice) => {
                let collisions = std::mem::take(&mut state.collisions);
                let names: Vec<String> = state
                    .selected_names()
                    .into_iter()
                    .map(str::to_owned)
                    .filter(|name| match choice {
                        OverwriteChoice::OverwriteAll => true,
                        OverwriteChoice::SkipAll => !collisions.contains(name),
                        OverwriteChoice::Cancel => false,
                    })
                    .collect();
                let written = if names.is_empty() {
                    0
                } else {
                    self.export_presets(&names)
                };
                state.finished = Some(written > 0);
                false
            }
            PresetsAction::RevealExportFolder => {
                if let Err(err) = reveal_folder(&self.export_dir) {
                    log::warn!("could not open {}: {err}", self.export_dir.display());
                    self.raise_notice(tr_args(
                        "Presets exported to %s",
                        &[&self.export_dir.display().to_string()],
                    ));
                }
                false
            }
            PresetsAction::CloseExport => true,
            PresetsAction::ChooseImportFolder
            | PresetsAction::Import
            | PresetsAction::DismissNotice
            | PresetsAction::CloseImport => false,
        }
    }

    /// The presets among `names` whose file already exists in the export directory — the file the
    /// edit direction's store will write, asked of the store, so the question comes up for exactly
    /// the files an export would replace.
    fn export_collisions(&self, names: &[String]) -> Vec<String> {
        let store = self.store(self.state.direction);
        names
            .iter()
            .filter(|name| {
                store
                    .file_name(name)
                    .is_some_and(|file| self.export_dir.join(file).exists())
            })
            .cloned()
            .collect()
    }

    /// Write `names`, from the edit direction's store, into the export directory — each as last
    /// saved. Returns how many files were written, which is what `FxController::exportPresets()`
    /// reduces to a `bool`.
    fn export_presets(&mut self, names: &[String]) -> usize {
        if let Err(err) = std::fs::create_dir_all(&self.export_dir) {
            log::warn!("could not create {}: {err}", self.export_dir.display());
            self.raise_notice(tr("Could not create the export folder"));
            return 0;
        }
        let mut written = 0;
        let lane = self.state.direction;
        for name in names {
            match self.store(lane).export(name, &self.export_dir) {
                Ok(_) => written += 1,
                Err(err) => {
                    log::warn!("could not export {name}: {err}");
                    self.raise_notice(tr_args("Could not export %s", &[name.as_str()]));
                }
            }
        }
        written
    }

    /// Rebuild the preset list after the store changed underneath it, keeping the same preset
    /// selected (its index may have moved) and its unsaved-changes marker, which lives only in
    /// the UI state until the next autosave.
    fn refresh_preset_list_keeping_selection(&mut self) {
        let selected = self.state.preset().map(|p| (p.name.clone(), p.modified));
        self.refresh_preset_list();
        let Some((name, modified)) = selected else {
            return;
        };
        self.state.selected_preset = self.state.presets.iter().position(|p| p.name == name);
        if modified
            && let Some(index) = self.state.selected_preset
            && let Some(entry) = self.state.presets.get_mut(index)
        {
            entry.modified = true;
        }
        self.note_presets();
    }
}

/// A voice store whose factory set is `voices`, written into `factory` as the shipped files are,
/// with the user's voice presets in `user_dir` — for the tests of this crate, which need voice
/// presets on disk now that selecting one loads it from its store.
#[doc(hidden)]
#[must_use]
pub fn voice_store_for_tests(
    voices: &[InputPreset],
    factory: &Path,
    user_dir: PathBuf,
) -> InputPresetStore {
    if let Err(err) = std::fs::create_dir_all(factory) {
        log::warn!("{}: {err}", factory.display());
    }
    for voice in voices {
        match InputPresetStore::file_name(&voice.name) {
            Ok(file) => {
                if let Err(err) = voice.save(&factory.join(file)) {
                    log::warn!("{}: {err}", voice.name);
                }
            }
            Err(err) => log::warn!("{}: {err}", voice.name),
        }
    }
    let mut store = InputPresetStore::with_dirs(vec![factory.to_path_buf()], user_dir);
    store.rescan();
    store
}

/// The microphone snapshot before any voice preset is chosen: an 80 Hz second-order high-pass, a
/// flat equalizer, no makeup, and every dynamics stage off.
///
/// Not `InputDspParams::default()`, which is Clean Voice with its gate, compressor, de-esser and
/// 6 dB of makeup: nothing should start working on someone's voice before they have chosen it. The
/// window used to get the same effect by overwriting the snapshot from its own controls on every
/// change — whichever lane it was showing, which is how a music preset's equalizer and gain once
/// reached a microphone.
fn unvoiced_input_params() -> InputDspParams {
    InputDspParams {
        rnnoise: false,
        gate_on: false,
        compressor_on: false,
        deesser_on: false,
        makeup_db: 0.0,
        ..InputDspParams::default()
    }
}

/// The control-plane requests a freshly started engine is sent from the settings file, in order.
///
/// The engine is created once per process and remembers nothing of the last run, so anything the
/// settings file asks of it that is not carried by a parameter snapshot has to be said here:
///
/// - What a previous run displaced, first: if that run was killed while holding the default, the
///   metadata still names a node that is gone and only this can point it back at a real device.
/// - The speakers' lane, when it was left off. The engine starts with the output lane enabled —
///   FxSound in front of the speakers is the Windows behaviour — so `output_enabled = false` has
///   to be said before the device rules attach it, or the speakers would be processed after
///   every restart while the combo said `Off`. The input lane starts detached and needs no word:
///   an enabled one is attached by its saved device the first time the list names it
///   ([`saved_device_to_announce`]).
/// - The per-device volumes of FxSound's own nodes (U10), so the first pair the engine builds
///   for a device starts at the level the user left it at rather than at whatever WirePlumber
///   restores. Sent even when there are none: it is the engine's whole memory of them, and "none"
///   is an answer too.
/// - Each lane's device ranking (U4, [`crate::priority::ranking`]), before the engine's first
///   choice of device, which would otherwise be made by the Windows rules alone. Empty while the
///   user has FxSound follow the system's default device.
/// - With the power left off, both lanes' hand-back of the session default (U12): FxSound off is
///   FxSound out of the path, and the engine, which starts wanting the default, would otherwise
///   take it with the first pair it builds.
/// - Echo cancellation, when it was left on. It is otherwise sent only when the checkbox is
///   toggled, so a saved `echo_cancel = true` came back as a pane and a strip saying it had been
///   asked for, while the engine was never told and `module-echo-cancel` never loaded. Off is the
///   engine's own starting state and is not repeated.
fn startup_messages(settings: &Settings) -> Vec<UiToAudio> {
    let mut messages = vec![
        UiToAudio::SeedRememberedDefaults {
            output: settings.remembered_default_output.clone(),
            input: settings.remembered_default_input.clone(),
        },
        UiToAudio::SeedTargetVolumes(settings.device_volumes.clone()),
    ];
    messages.extend(
        DeviceDirection::ALL.map(|direction| UiToAudio::SetDevicePriority {
            direction,
            names: priority::ranking(settings, direction),
        }),
    );
    if !settings.power {
        messages.extend(default_claims(false));
    }
    if !settings.lane_enabled(DeviceDirection::Output) {
        messages.push(UiToAudio::DetachLane(DeviceDirection::Output));
    }
    if settings.echo_cancel {
        messages.push(UiToAudio::SetEchoCancel(true));
    }
    messages
}

/// Both lanes' claim on the session default, taken (`want`) or handed back — what the power switch
/// does to the path the system's sound takes (U12).
///
/// Both lanes, not only the enabled ones: a lane switched off has nothing to hand back, and the
/// engine keeps the wish for when it is switched on again, so a microphone picked while the power
/// is off does not take the default source from the system either.
fn default_claims(want: bool) -> [UiToAudio; 2] {
    DeviceDirection::ALL.map(|direction| UiToAudio::SetAsDefault { direction, want })
}

/// The action that attaches `lane` to device `index`.
pub(crate) const fn select_on(lane: DeviceDirection, index: usize) -> UiAction {
    match lane {
        DeviceDirection::Output => UiAction::SelectOutput(index),
        DeviceDirection::Input => UiAction::SelectInput(index),
    }
}

/// The action that detaches `lane`.
pub(crate) const fn detach(lane: DeviceDirection) -> UiAction {
    match lane {
        DeviceDirection::Output => UiAction::DetachOutput,
        DeviceDirection::Input => UiAction::DetachInput,
    }
}

/// An equalizer as the window holds it, from a snapshot's two parallel arrays.
/// The engine's band ladder for `count` bands: the original's hard-coded table where it has
/// one, else the geometric ladder `GraphicEq` builds.
fn ladder(count: usize) -> Vec<f32> {
    fxsound_dsp::eq::band_table(count).map_or_else(
        || {
            let mut eq = fxsound_dsp::GraphicEq::new();
            eq.set_num_bands(count);
            eq.center_frequencies().to_vec()
        },
        |(table, _, _)| table.to_vec(),
    )
}

fn bands_of(centres: &[f32], boosts: &[f32]) -> Vec<EqBand> {
    centres
        .iter()
        .zip(boosts)
        .map(|(&center_hz, &boost_db)| EqBand {
            center_hz,
            boost_db,
        })
        .collect()
}

/// Copy what the engine's lanes measured into what the window draws.
///
/// The picture — the spectrum, whether anything is playing, and the rate that decides which
/// equalizer bands can be built — is `shown`'s, the meters of the lane the window edits. The
/// microphone's telemetry is always `microphone`'s, the input lane's: the readout strip reads it
/// whichever lane the window shows (0.4.0 design §1.4), and one engine's meters used to hand the
/// music chain's zeros to the strip the moment the speakers were edited.
fn show_meters(state: &mut UiState, shown: &Meters, microphone: &Meters) {
    state.spectrum = shown.spectrum;
    state.audio_active = shown.active;
    // The rate is the device's, and the interface needs it for one thing: an equalizer band
    // centred at or above Nyquist cannot be built, and a fader that does nothing has to say so. A
    // 16 kHz Bluetooth capture takes the top two bands of the standard ladder with it.
    state.sample_rate = shown.sample_rate;

    state.gate_reduction_db = microphone.gate_reduction_db;
    state.deesser_running = microphone.deesser_running;
    state.denoise_running = microphone.denoiser_running;
    state.voice_probability = microphone.voice_probability;
    state.compressor_reduction_db = microphone.compressor_reduction_db;
    state.deesser_reduction_db = microphone.deesser_reduction_db;
    // Copied as it is: `0.0` is what no measurement looks like (`Meters::default()`, and the
    // running minimum before it has come down), and the strip reads it as a dash rather than as a
    // room at full scale.
    state.noise_floor_db = microphone.noise_floor_db;
    state.denoise_reduction_db = microphone.denoise_reduction_db;
    state.dereverb_reduction_db = microphone.dereverb_reduction_db;
    state.deesser_hz = microphone.deesser_hz;
}

/// Whether a lane's last status said it was processing (see [`set_lane_active`]).
const fn lane_running(state: &UiState, direction: DeviceDirection) -> bool {
    match direction {
        DeviceDirection::Output => state.output_active,
        DeviceDirection::Input => state.input_active,
    }
}

/// Record whether a lane is processing, for the strip's Floor slot, `--status` and `--watch`.
fn set_lane_active(state: &mut UiState, direction: DeviceDirection, processing: bool) {
    match direction {
        DeviceDirection::Output => state.output_active = processing,
        DeviceDirection::Input => state.input_active = processing,
    }
}

/// Which device a lane's combo shows.
///
/// The engine's word, never a guess from the device list. Nothing at all for a lane the user
/// switched off: that is what `Off` means. Otherwise the device the lane has been asked to attach
/// to and the engine has not answered about yet (`requested`: a pick, or the saved device
/// announced at start-up), since that is where the lane is going — whatever it is attached to
/// now, and whatever device list or news about the other lane arrives before the answer. The
/// lane's next [`AudioToUi::Attached`] or error ends the request, so it never outlives the
/// engine's answer. Without one, the device the engine says the lane is attached to (`attached`)
/// — the engine's device rules can move a lane on their own, when its device is unplugged or a
/// better one is plugged in, and the combo says where the sound really goes. With neither,
/// nothing: an enabled lane that is attached to nothing processes nothing, and 0.3.0's fallback
/// to the server's default device showed a device FxSound was not in front of. Either name counts
/// only when the list carries it in the lane's direction, so a request for a device that has
/// gone shows where the lane still is.
///
/// With the power off, FxSound is out of the path (U12): once a request has been answered, the
/// lane shows the system's default device of its direction, which is where the sound goes, as
/// upstream's `syncOutputWithSystemDefault` does (`FxController.cpp:1666-1712`). A device picked
/// while the power is off is the one the lane takes the default for when it comes back on; it
/// shows until the engine has attached the lane to it. When the default is FxSound's own node
/// and no real device is marked, the device the lane is attached to is where the sound goes.
fn lane_selection(
    settings: &Settings,
    devices: &[AudioDevice],
    direction: DeviceDirection,
    attached: Option<&str>,
    requested: Option<&str>,
) -> Option<usize> {
    if !settings.lane_enabled(direction) {
        return None;
    }
    let listed = |name: &str| {
        devices
            .iter()
            .position(|d| d.name == name && d.direction == direction)
    };
    let system_default = || {
        devices
            .iter()
            .position(|d| d.is_default && d.direction == direction)
    };
    let requested = requested.and_then(listed);
    if !settings.power {
        return requested
            .or_else(system_default)
            .or_else(|| attached.and_then(listed));
    }
    requested.or_else(|| attached.and_then(listed))
}

/// A lane's saved device selection, when it is time to send it to the engine.
///
/// The port of `FxController::init` step 5 (`docs/spec/05-controller-model.md` §9.2): once the
/// device list is known, a device named by the saved `output_device_name` is adopted and
/// `setOutput` forces it. Here the engine starts with the output lane enabled and runs the device
/// rules on its own, so each lane's saved choice — `settings.device_name(direction)` — is handed
/// to it the first time that device is listed; rule 2 of `choose_device` yields to it
/// (`docs/spec/12-audio-io.md` §28.5). A lane the settings say is off
/// ([`Settings::lane_enabled`]) is not announced at all: a saved microphone switches the input
/// lane on beside the speakers only when it was on when FxSound last ran, and a speakers' lane
/// left off was detached at start-up ([`startup_messages`]) and stays so.
///
/// `announced` is what was last sent for this lane. A device list that merely changed *around*
/// the saved device sends nothing again; a device that vanished and came back is announced afresh,
/// which the engine's own rules make a no-op when the lane is already attached to it. An empty
/// saved name never matches a node, so nothing is sent on a first run.
fn saved_device_to_announce(
    settings: &Settings,
    devices: &[AudioDevice],
    direction: DeviceDirection,
    announced: &mut Option<String>,
) -> Option<UiToAudio> {
    let wanted = settings.device_name(direction);
    let listed = devices
        .iter()
        .any(|d| d.name == wanted && d.direction == direction);
    if !listed || !settings.lane_enabled(direction) {
        *announced = None;
        return None;
    }
    if announced.as_deref() == Some(wanted) {
        return None;
    }
    *announced = Some(wanted.to_owned());
    Some(UiToAudio::SelectDevice {
        node_name: wanted.to_owned(),
        direction,
    })
}

/// Where a lane's entry sits in the `[_; 2]` tables the controller keeps per direction: outputs
/// first, the order of [`DeviceDirection::ALL`].
const fn lane_index(direction: DeviceDirection) -> usize {
    match direction {
        DeviceDirection::Output => 0,
        DeviceDirection::Input => 1,
    }
}

/// `~/Documents/FxSound/Presets/Export`, falling back to the home directory when the XDG user
/// directories are not configured.
fn default_export_dir() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("FxSound")
        .join("Presets")
        .join("Export")
}

/// `File::revealToUser()` (`FxPresetExportDialog.cpp:196`): open the export folder in whatever
/// the desktop uses for folders.
///
/// The portal route (`org.freedesktop.portal.OpenURI.OpenDirectory`) needs a D-Bus crate this
/// crate does not depend on, so this is the fallback `docs/spec/06-dialogs.md` §9.3 names:
/// `xdg-open <dir>`, which dispatches to the user's own file manager rather than a hardcoded one.
/// The child is reaped on a helper thread so it never lingers as a zombie.
fn reveal_folder(dir: &Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("xdg-open")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::Builder::new()
        .name("fxsound-reveal".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map(|_| ())
}

impl App {
    /// The persisted settings, for the Settings window to edit a copy of.
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The Settings pane's working copy, filled in with everything it shows: the version, the
    /// autostart state read live from the entry (as the original reads the registry,
    /// `FxController.cpp:2789-2812`), the preset names and the remembered devices.
    #[must_use]
    pub fn settings_state(&self) -> SettingsState {
        let mut state = SettingsState::new(self.settings.clone());
        state.version = env!("CARGO_PKG_VERSION").to_owned();
        state.launch_on_startup = autostart_enabled();
        // The rows' preset combos offer the speakers' presets, whichever lane the window edits:
        // the list is the Output Device Preference.
        state.presets = self
            .presets
            .entries()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        // The reset button is enabled iff there is something to lose
        // (`FxSettingsDialog.cpp:210-220`), on either lane, since the reset covers both.
        state.can_reset_presets = DeviceDirection::ALL.into_iter().any(|lane| {
            self.lane_preset(lane).is_some_and(|(_, modified)| modified)
                || self
                    .store(lane)
                    .entries()
                    .iter()
                    .any(|p| p.source != fxsound_preset::PresetSource::Factory || p.modified)
        });
        state.devices = self.device_rows(DeviceDirection::Output);
        state.microphones = self.device_rows(DeviceDirection::Input);
        self.refresh_settings_state(&mut state);
        state
    }

    /// One direction's priority list as the Settings pane draws it, most preferred first: the
    /// Audio pane's Output Device Preference (`FxOutputPreference.cpp:104-181`), each row with its
    /// `.fac` preset, or the Microphone pane's Input Device Preference (U4).
    ///
    /// A microphone's row has no preset combo: the voice preset it remembers is not in the
    /// speakers' list the combo offers — offered one, it would remember a `.fac` it can never
    /// load. Rows count one direction's devices only; the actions that name a row find its entry
    /// through [`App::config_index`].
    fn device_rows(&self, direction: DeviceDirection) -> Vec<DevicePriority> {
        let playing = self.state.device_for(direction).map(|d| d.name.as_str());
        priority::ranked(&self.settings, direction)
            .map(|config| DevicePriority {
                id: config.device_id.clone(),
                name: config.device_name.clone(),
                preset: match direction {
                    DeviceDirection::Output => self.presets.index_of(&config.preset),
                    DeviceDirection::Input => None,
                },
                connected: playing == Some(config.device_id.as_str()),
                present: self
                    .state
                    .devices
                    .iter()
                    .any(|d| d.direction == direction && d.name == config.device_id),
            })
            .collect()
    }

    /// The `device_configs` entry that `direction`'s list shows at `row`.
    fn config_index(&self, direction: DeviceDirection, row: usize) -> Option<usize> {
        self.settings
            .device_configs
            .iter()
            .enumerate()
            .filter(|(_, config)| config.direction == direction)
            .nth(row)
            .map(|(index, _)| index)
    }

    /// The priority list changed in the open pane: copy it over, rows and all, save it, and put
    /// it in force — the window's and the tray's lists in its order, the engine told
    /// ([`App::device_priority_changed`]).
    fn device_configs_changed(&mut self, state: &mut fxsound_ui::dialogs::settings::SettingsState) {
        self.persist_settings();
        self.device_priority_changed();
        self.refresh_device_rows(state);
    }

    /// Bring the open pane's copy of the priority list, and both lists' rows, up to date — after
    /// an action, and every frame, since a device list arriving while the pane is open adds to
    /// the list and changes which rows are present.
    fn refresh_device_rows(&self, state: &mut SettingsState) {
        if state.settings.device_configs != self.settings.device_configs {
            state
                .settings
                .device_configs
                .clone_from(&self.settings.device_configs);
        }
        let outputs = self.device_rows(DeviceDirection::Output);
        if state.devices != outputs {
            state.devices = outputs;
        }
        let microphones = self.device_rows(DeviceDirection::Input);
        if state.microphones != microphones {
            state.microphones = microphones;
        }
        if state
            .selected_device
            .is_some_and(|row| row >= state.devices.len())
        {
            state.selected_device = None;
        }
    }

    /// Settings ▸ Reset Presets: every preset of **both** lanes back to what shipped or was last
    /// saved, by dropping every autosave in both stores.
    ///
    /// Each lane then loads the preset its device remembers (upstream 7f160b6), or else its
    /// selected one again as saved — the lane in the window and the one off screen alike, so the
    /// engine stops running edits that no longer exist anywhere, rather than the lane off screen
    /// keeping them until it is next looked at.
    fn reset_presets(&mut self) {
        for lane in DeviceDirection::ALL {
            let names: Vec<String> = self
                .store(lane)
                .entries()
                .iter()
                .map(|entry| entry.name.clone())
                .collect();
            let store = self.store_mut(lane);
            for name in &names {
                store.clear_autosave(name);
            }
            store.rescan();
        }
        for lane in DeviceDirection::ALL {
            self.in_lane(lane, |app| {
                app.refresh_preset_list_keeping_selection();
                if let Some(index) = app.device_preset_index().or(app.state.selected_preset) {
                    app.select_preset(index);
                }
            });
        }
    }

    /// `--language <code>` from the command line: an explicit pick, or `system`/`default` to
    /// follow the desktop again (`FxController.cpp:269-278` only knew codes).
    pub fn set_language(&mut self, code: &str) {
        let choice = match code.trim().to_ascii_lowercase().as_str() {
            "system" | "default" | "" => None,
            _ => Some(code.trim()),
        };
        self.settings.choose_language(choice);
        i18n::set_language(self.settings.effective_language());
        // The tray's labels are built in the language in force.
        self.tray_stale = true;
        self.persist_settings();
    }

    /// Hand a toast to the desktop, unless notifications are hidden or start-up is still going.
    fn notify(&self, message: Message) {
        if self.notifications_armed {
            let _ = self.notifier.notify(message);
        }
    }

    /// `"FxSound is on."` / `"FxSound is off."` — the command-line and keybind path's toast
    /// (`FxController.cpp:1933`); the window's own power button says nothing.
    pub fn notify_power(&self) {
        self.notify(Message::power_toggled(self.state.power));
    }

    /// The "FxSound in system tray" tip, once per process, the first time the window hides
    /// (`FxController.cpp:920-926`).
    pub fn notify_hidden_to_tray(&mut self, tray_visible: bool) {
        if self.tray_tip_shown {
            return;
        }
        self.tray_tip_shown = true;
        if tray_visible {
            self.notify(Message::minimised_to_tray());
        } else {
            // "Click FxSound icon to reopen" is not advice on a session with no icon. This is the
            // GNOME-without-AppIndicator case, and the honest version names the way back.
            log::warn!(
                "hiding with no tray icon in this session; the window can be brought back with \
                 `fxsound --show`"
            );
            self.notify(Message::hidden_with_no_tray());
        }
    }

    /// Act on one Settings-window action.
    ///
    /// `state` is the dialog's own working copy; anything that has to outlive the window is
    /// written back into the real settings here and saved immediately, which is what the original
    /// does — the Settings dialog has no OK/Cancel (`FxSettingsDialog.cpp`).
    pub fn handle_settings(
        &mut self,
        action: &fxsound_ui::dialogs::settings::SettingsAction,
        state: &mut fxsound_ui::dialogs::settings::SettingsState,
    ) {
        use fxsound_ui::dialogs::settings::SettingsAction as A;
        match action {
            A::SelectTab(tab) => state.tab = *tab,

            A::SetPrioritizeNewOutput(on) => {
                self.settings.prioritize_new_output = *on;
                state.settings.prioritize_new_output = *on;
                self.persist_settings();
            }
            // Upstream issue #629: the priority list stops choosing the device, and the system's
            // default does, as the Windows rules have it. The list itself is kept.
            A::SetFollowSystemDefault(on) => {
                self.settings.follow_system_default = *on;
                state.settings.follow_system_default = *on;
                self.persist_settings();
                self.device_priority_changed();
            }
            A::SetLanguage(choice) => {
                // Live: every view asks `i18n::tr` on every frame, so the swap is visible on the
                // next one. The PipeWire node descriptions keep the language they were created
                // in — rebuilding both nodes for a caption would drop the audio for a moment.
                self.settings.choose_language(choice.as_deref());
                i18n::set_language(self.settings.effective_language());
                self.tray_stale = true;
                state.settings.language.clone_from(&self.settings.language);
                state.settings.language_follows_system = self.settings.language_follows_system;
                self.persist_settings();
            }
            A::SetHideHelpTips(on) => {
                self.settings.hide_help_tooltips = *on;
                state.settings.hide_help_tooltips = *on;
                self.state.hide_tooltips = *on;
                self.persist_settings();
            }
            A::SetHideNotifications(on) => {
                self.settings.hide_notifications = *on;
                state.settings.hide_notifications = *on;
                self.notifier.set_hidden(*on);
                self.persist_settings();
            }
            A::SetLaunchOnStartup(on) => {
                // The Windows build writes an HKCU\...\Run value; the XDG equivalent is a
                // .desktop file in ~/.config/autostart (`docs/spec/07-startup-tray.md`).
                match set_autostart(*on) {
                    Ok(()) => state.launch_on_startup = *on,
                    Err(err) => {
                        log::warn!("could not change the autostart entry: {err}");
                        self.raise_notice(tr("Could not change the startup setting"));
                    }
                }
            }

            // The rows are one direction's devices only (see `device_rows`), so a row is found
            // among them, never by its place in the whole list.
            A::SelectDeviceRow(row) => {
                state.selected_device = (*row < state.devices.len()).then_some(*row);
            }
            A::RemoveDevice(row) => self.remove_device_config(state, DeviceDirection::Output, *row),
            // The moved row stays the selected one, so Shift+Up can carry it on up
            // (`FxOutputPreference.cpp:238-260`, `onRowMoved`).
            A::MoveDeviceUp(row) => {
                if let Some(above) = row.checked_sub(1)
                    && self.swap_device_config(state, DeviceDirection::Output, *row, above)
                {
                    state.selected_device = Some(above);
                }
            }
            A::MoveDeviceDown(row) => {
                if self.swap_device_config(state, DeviceDirection::Output, *row, row + 1) {
                    state.selected_device = Some(row + 1);
                }
            }
            A::RemoveMicrophone(row) => {
                self.remove_device_config(state, DeviceDirection::Input, *row);
            }
            A::MoveMicrophoneUp(row) => {
                if let Some(above) = row.checked_sub(1) {
                    self.swap_device_config(state, DeviceDirection::Input, *row, above);
                }
            }
            A::MoveMicrophoneDown(row) => {
                self.swap_device_config(state, DeviceDirection::Input, *row, row + 1);
            }
            A::SetDevicePreset { device, preset } => {
                // A name from the speakers' store, the list the combo offers.
                let name = self.presets.entries().get(*preset).map(|p| p.name.clone());
                if let (Some(index), Some(name)) =
                    (self.config_index(DeviceDirection::Output, *device), name)
                {
                    self.settings.device_configs[index].preset = name;
                    // The row of the device playing now takes effect at once, rather than the
                    // next time the device comes back (upstream 83ccf5e,
                    // `FxOutputPreference.cpp:57-61`).
                    let node_name = self.settings.device_configs[index].device_id.clone();
                    if self
                        .state
                        .device_for(DeviceDirection::Output)
                        .is_some_and(|playing| playing.name == node_name)
                    {
                        self.bring_back_device_preset(DeviceDirection::Output, &node_name);
                    }
                    self.device_configs_changed(state);
                }
            }

            A::ResetPresets => {
                self.reset_presets();
                let message = Message::presets_restored();
                self.raise_notice(message.body.clone());
                self.notify(message);
            }

            A::OpenUrl(url) => {
                if let Err(err) = open_url(url) {
                    log::warn!("could not open {url}: {err}");
                    self.raise_notice(tr("Could not open the link"));
                }
            }

            // The microphone pane (0.4.0 design §1.4). Each is written, saved, and published to
            // the voice chain at once, whichever lane the window is editing.
            A::SetNoiseSuppression(choice) => {
                self.settings.noise_suppression = *choice;
                state.settings.noise_suppression = *choice;
                self.microphone_setting_changed();
            }
            A::SetDenoiseChannels(choice) => {
                self.settings.denoise_channels = *choice;
                state.settings.denoise_channels = *choice;
                self.microphone_setting_changed();
            }
            A::SetDeEsserMode(mode) => {
                self.settings.deesser_mode = *mode;
                state.settings.deesser_mode = *mode;
                self.microphone_setting_changed();
            }
            A::SetDereverb(level) => {
                self.settings.dereverb = *level;
                state.settings.dereverb = *level;
                self.microphone_setting_changed();
            }
            A::SetEchoCancel(on) => {
                self.settings.echo_cancel = *on;
                state.settings.echo_cancel = *on;
                // A module the audio thread loads, not a stage in the snapshot; it answers with
                // `AudioToUi::EchoCancel`, which is how "unavailable" reaches the pane.
                if let Some(engine) = &self.engine {
                    engine.send(UiToAudio::SetEchoCancel(*on));
                }
                self.microphone_setting_changed();
            }

            // The window layer owns these: it has the viewport, the hotkey example is a
            // dialog-local hint, and the changelog and the calibration wizard are panes of their
            // own.
            A::ShowHotkeyExample | A::ShowChangelog | A::OpenCalibration | A::Close => {}
        }
    }

    /// Swap two rows of `direction`'s priority list — two devices' places in `device_configs`,
    /// whatever entries of the other direction sit between them. Returns whether both rows exist.
    fn swap_device_config(
        &mut self,
        state: &mut fxsound_ui::dialogs::settings::SettingsState,
        direction: DeviceDirection,
        a: usize,
        b: usize,
    ) -> bool {
        let (Some(a), Some(b)) = (
            self.config_index(direction, a),
            self.config_index(direction, b),
        ) else {
            return false;
        };
        self.settings.device_configs.swap(a, b);
        self.device_configs_changed(state);
        true
    }

    /// Forget a device the list shows at `row` (`FxOutputPreference.cpp:262-272`). The pane
    /// offers the ✕ only for a device that is not there; one that is comes back at the bottom of
    /// the list, or the top, with the next device list, as upstream's would.
    fn remove_device_config(
        &mut self,
        state: &mut fxsound_ui::dialogs::settings::SettingsState,
        direction: DeviceDirection,
        row: usize,
    ) {
        if let Some(index) = self.config_index(direction, row) {
            self.settings.device_configs.remove(index);
            self.device_configs_changed(state);
        }
    }

    fn persist_settings(&mut self) {
        if self.persist
            && let Err(err) = self.settings.save()
        {
            log::warn!("could not save settings: {err}");
        }
    }

    /// A microphone setting moved: save it, and publish the voice chain with it applied.
    fn microphone_setting_changed(&mut self) {
        self.persist_settings();
        self.sync_params_from_state();
    }

    /// Bring the Settings pane's live fields up to date — what the audio thread has said about the
    /// echo canceller since the pane opened, whether there is still a microphone to calibrate, and
    /// a noise-suppression level `--noise-suppression` set while it was open. The host calls this
    /// every frame the pane is open; everything else in the pane's working copy changes only
    /// through [`App::handle_settings`].
    pub fn refresh_settings_state(&self, state: &mut SettingsState) {
        self.refresh_device_rows(state);
        state.echo_cancel_running = self.state.echo_cancel_running;
        // The one microphone setting the command line and D-Bus can change under an open pane.
        state.settings.noise_suppression = self.settings.noise_suppression;
        if state.echo_cancel_detail != self.echo_cancel_detail {
            state
                .echo_cancel_detail
                .clone_from(&self.echo_cancel_detail);
        }
        state.has_microphone = self.microphone_description().is_some();
        // A calibration applied while the pane is open is the pane's last-calibration line.
        if state.settings.calibration != self.settings.calibration {
            state
                .settings
                .calibration
                .clone_from(&self.settings.calibration);
        }
    }

    /// The description of the microphone the input lane is attached to, which the calibration
    /// wizard shows under its title, or `None` while the lane has no microphone.
    ///
    /// What the engine says the lane is on ([`App::attached`]), not the combo: the wizard measures
    /// the lane's own meters, and a microphone that has been picked but not attached yet has none.
    #[must_use]
    pub fn microphone_description(&self) -> Option<&str> {
        let attached = self.attached(DeviceDirection::Input)?;
        self.state
            .devices
            .iter()
            .find(|device| device.direction == DeviceDirection::Input && device.name == attached)
            .map(|device| device.description.as_str())
    }
}

// =============================================================================================
// The calibration wizard (0.4.0 design §8)
// =============================================================================================

impl App {
    /// The input lane as the wizard sees it: the microphone the engine says it is attached to, as
    /// the device list describes it, whether it is processing, its channels, and `meters`.
    fn calibration_lane<'a>(&'a self, meters: &'a Meters) -> Lane<'a> {
        let microphone = self
            .attached(DeviceDirection::Input)
            .and_then(|attached| {
                self.state.devices.iter().find(|device| {
                    device.direction == DeviceDirection::Input && device.name == attached
                })
            })
            .map(|device| (device.name.as_str(), device.description.as_str()));
        Lane {
            microphone,
            processing: self.state.input_active,
            channels: self.audio_status_for(DeviceDirection::Input).channels,
            meters,
        }
    }

    /// Settings ▸ Microphone ▸ "Calibrate microphone…": open the wizard on its introduction, for
    /// the microphone the input lane is attached to. Returns whether the wizard is open — not
    /// without a microphone, which is when the pane's button is disabled.
    pub fn open_calibration(&mut self) -> bool {
        if self.calibration.is_none() {
            let meters = Meters::default();
            let lane = self.calibration_lane(&meters);
            if lane.microphone.is_none() {
                return false;
            }
            let wizard = CalibrationState::open(&lane, Instant::now());
            self.calibration = Some(wizard);
        }
        true
    }

    /// Whether the calibration wizard is open.
    #[must_use]
    pub const fn calibration_open(&self) -> bool {
        self.calibration.is_some()
    }

    /// Whether the wizard is waking the microphone, measuring or analysing: the window should
    /// redraw at the meters' pace, for the countdown and the live level.
    #[must_use]
    pub fn calibration_is_live(&self) -> bool {
        self.calibration
            .as_ref()
            .is_some_and(CalibrationState::is_live)
    }

    /// What the wizard draws now, while it is open.
    #[must_use]
    pub fn calibration_view(&self) -> Option<CalibrationView> {
        self.calibration_view_at(Instant::now())
    }

    pub(crate) fn calibration_view_at(&self, now: Instant) -> Option<CalibrationView> {
        self.calibration.as_ref().map(|wizard| wizard.view(now))
    }

    /// What the user pressed in the wizard: Start and Retry begin a run, Apply writes the result
    /// and closes, Cancel and Close let the microphone go and close.
    pub fn handle_calibration(&mut self, action: CalibrationAction) {
        self.calibration_action_at(action, Instant::now());
    }

    pub(crate) fn calibration_action_at(&mut self, action: CalibrationAction, now: Instant) {
        match action {
            CalibrationAction::Start | CalibrationAction::Retry => {
                if let Some(wizard) = self.calibration.as_mut() {
                    let commands = wizard.start(now);
                    self.carry_out_calibration(commands);
                }
            }
            CalibrationAction::Cancel | CalibrationAction::Close => self.cancel_calibration(),
            CalibrationAction::Apply => {
                let Some(result) = self
                    .calibration
                    .as_ref()
                    .and_then(CalibrationState::result)
                    .cloned()
                else {
                    return;
                };
                // A result that could not be written leaves the wizard on it, with the notice
                // that says why, so Close is still the user's to press.
                if self.apply_calibration(&result) {
                    self.cancel_calibration();
                }
            }
        }
    }

    /// Close the wizard, letting go of the microphone if it was being held: Cancel at any phase,
    /// the window going away, the application quitting.
    pub fn cancel_calibration(&mut self) {
        if let Some(mut wizard) = self.calibration.take() {
            let commands = wizard.cancel();
            self.carry_out_calibration(commands);
        }
    }

    /// One look at the input lane for an open wizard, from [`App::poll_audio`].
    fn drive_calibration(&mut self, now: Instant, meters: &Meters) {
        let Some(mut wizard) = self.calibration.take() else {
            return;
        };
        let commands = wizard.tick(now, &self.calibration_lane(meters));
        self.calibration = Some(wizard);
        self.carry_out_calibration(commands);
    }

    /// Tell the engine what the wizard asked for.
    fn carry_out_calibration(&self, commands: Vec<CalibrationCommand>) {
        let Some(engine) = &self.engine else {
            return;
        };
        for command in commands {
            match command {
                CalibrationCommand::KeepInputAwake(awake) => {
                    engine.send(UiToAudio::KeepInputAwake(awake));
                }
                CalibrationCommand::ResetCaptureStats => {
                    engine.send_event(DeviceDirection::Input, DspEvent::ResetCaptureStats);
                }
            }
        }
    }

    /// Apply: write `calibration` as the user voice preset `Calibrated — <device>` over the
    /// shipped preset it recommends, select it on the microphone's lane whichever lane the window
    /// is editing, and keep the record in the settings file. Returns whether the preset was
    /// written.
    ///
    /// A second calibration of the same microphone overwrites the first, keeping a `.bak`, as a
    /// save over a user preset does. A new one is refused at the user-preset cap, as Save New
    /// Preset is.
    fn apply_calibration(&mut self, calibration: &Calibration) -> bool {
        use fxsound_preset::PresetSource;

        let name = calibration::calibrated_preset_name(&calibration.microphone.description);
        let overwrite = self
            .voice_presets
            .find(&name)
            .is_some_and(|entry| entry.source == PresetSource::User);
        let user_presets = self
            .voice_presets
            .entries()
            .iter()
            .filter(|entry| entry.source == PresetSource::User)
            .count();
        if !overwrite && user_presets >= self.max_user_presets() {
            let message = Message::preset_limit_reached();
            self.raise_notice(message.body.clone());
            self.notify(message);
            return false;
        }

        let shipped = calibration.recommendation.preset;
        let base = self
            .voice_presets
            .load_saved(shipped)
            .unwrap_or_else(|err| {
                // Clean Voice's numbers under the recommended name: the calibrated preset is still
                // a whole one, and the numbers the wizard measured are the point of it.
                log::warn!("calibration: {err}; starting from the built-in voice");
                InputPreset {
                    name: shipped.to_owned(),
                    ..InputPreset::default()
                }
            });
        let preset = calibration
            .recommendation
            .preset(base, &name, calibration.description());
        if let Err(err) = self.voice_presets.save_as(&preset, &name) {
            log::warn!("could not save the calibrated preset {name}: {err}");
            self.raise_notice(tr_args("Could not save %s", &[name.as_str()]));
            return false;
        }

        self.in_lane(DeviceDirection::Input, |app| {
            app.refresh_preset_list_keeping_selection();
            if let Some(index) = app.state.presets.iter().position(|p| p.name == name) {
                app.select_preset(index);
            }
        });

        let measured = &calibration.measurement;
        self.record_calibration(CalibrationRecord {
            noise_floor_db: measured.floor_db,
            speech_rms_db: measured.speech_rms_db,
            speech_peak_db: measured.speech_peak_db,
            clipped_ratio: measured.clipped_ratio,
            unix_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            preset: name.clone(),
            device: calibration.microphone.node_name.clone(),
        });
        let message = if overwrite {
            Message::preset_overwritten(&name)
        } else {
            Message::preset_saved(&name)
        };
        self.raise_notice(message.body.clone());
        self.notify(message);
        self.save_settings_if_dirty();
        true
    }
}

/// `~/.config/autostart/fxsound.desktop` — the XDG Autostart entry that stands in for the
/// `HKCU\…\CurrentVersion\Run` value (`FxController.cpp:2789-2812`).
fn autostart_path() -> std::io::Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| std::io::Error::other("no config directory"))?
        .join("autostart")
        .join("fxsound.desktop"))
}

/// Whether FxSound starts with the session: the entry exists and is not `Hidden=true`, which is
/// how a desktop's own autostart editor disables one without deleting it.
#[must_use]
pub fn autostart_enabled() -> bool {
    autostart_path()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|text| !text.lines().any(|line| line.trim() == "Hidden=true"))
}

/// Hand a URL to the desktop. Never fatal — a missing `xdg-open` is not worth a crash.
fn open_url(url: &str) -> std::io::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
}

/// Create or remove `~/.config/autostart/fxsound.desktop`.
fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let path = autostart_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no autostart directory"))?
        .to_path_buf();

    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        };
    }

    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        &path,
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=FxSound\n\
         Comment=Start FxSound minimised to the system tray on login\n\
         Exec=fxsound --hide\n\
         Icon=fxsound\n\
         Terminal=false\n\
         Categories=AudioVideo;Audio;\n\
         X-GNOME-Autostart-enabled=true\n",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless() -> App {
        App::headless_for_tests()
    }

    fn with_presets(names: &[&str]) -> App {
        let mut app = headless();
        app.state.presets = names
            .iter()
            .map(|n| PresetEntry {
                name: (*n).to_owned(),
                factory: true,
                modified: false,
            })
            .collect();
        app.state.selected_preset = Some(0);
        app
    }

    #[test]
    fn toggling_power_reaches_the_dsp_snapshot() {
        let mut app = headless();
        assert!(app.params().power);
        app.handle(&[UiAction::TogglePower]);
        assert!(!app.state.power);
        assert!(!app.params().power);
        app.handle(&[UiAction::TogglePower]);
        assert!(app.params().power);
    }

    #[test]
    fn an_effect_slider_is_converted_from_the_gui_scale_to_the_engine_scale() {
        let mut app = headless();
        app.handle(&[UiAction::SetEffect(Effect::Bass, 10.0)]);
        assert_eq!(app.state.effect(Effect::Bass), 10.0);
        assert!((app.params().effect(Effect::Bass) - 1.0).abs() < 1e-6);

        app.handle(&[UiAction::SetEffect(Effect::Bass, 5.0)]);
        assert!((app.params().effect(Effect::Bass) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn effect_values_are_clamped_to_the_slider_range() {
        let mut app = headless();
        app.handle(&[UiAction::SetEffect(Effect::Fidelity, 99.0)]);
        assert_eq!(app.state.effect(Effect::Fidelity), 10.0);
        app.handle(&[UiAction::SetEffect(Effect::Fidelity, -5.0)]);
        assert_eq!(app.state.effect(Effect::Fidelity), 0.0);
    }

    #[test]
    fn a_band_gain_is_clamped_to_twelve_decibels_either_way() {
        let mut app = headless();
        app.handle(&[UiAction::SetBandGain(0, 99.0)]);
        assert_eq!(app.state.eq_bands[0].boost_db, 12.0);
        app.handle(&[UiAction::SetBandGain(0, -99.0)]);
        assert_eq!(app.state.eq_bands[0].boost_db, -12.0);
    }

    #[test]
    fn a_band_gain_out_of_range_is_ignored_rather_than_panicking() {
        let mut app = headless();
        app.handle(&[UiAction::SetBandGain(999, 6.0)]);
        assert!(app.state.eq_bands.iter().all(|b| b.boost_db == 0.0));
    }

    #[test]
    fn changing_a_control_marks_the_preset_modified() {
        let mut app = with_presets(&["Jazz", "Rock"]);
        assert!(!app.state.presets[0].modified);
        app.handle(&[UiAction::SetEffect(Effect::Ambience, 4.0)]);
        assert!(app.state.presets[0].modified);
    }

    #[test]
    fn the_master_gain_and_balance_are_clamped_to_twenty_decibels() {
        let mut app = headless();
        app.handle(&[UiAction::SetMasterGain(99.0), UiAction::SetBalance(-99.0)]);
        assert_eq!(app.state.master_gain_db, 20.0);
        assert_eq!(app.state.balance_db, -20.0);
        assert_eq!(app.params().master_gain_db, 20.0);
        assert_eq!(app.params().balance, -20.0);
    }

    #[test]
    fn the_filter_width_knob_is_clamped_to_its_documented_range() {
        let mut app = headless();
        app.handle(&[UiAction::SetFilterQ(9.0)]);
        assert_eq!(app.state.filter_q, 3.0);
        app.handle(&[UiAction::SetFilterQ(0.0)]);
        assert_eq!(app.state.filter_q, 1.0);
    }

    fn gains(app: &App) -> Vec<f32> {
        app.state.eq_bands.iter().map(|b| b.boost_db).collect()
    }

    fn centres(app: &App) -> Vec<f32> {
        app.state.eq_bands.iter().map(|b| b.center_hz).collect()
    }

    #[test]
    fn restoring_defaults_keeps_the_curve_on_ten_bands_and_resets_the_levels() {
        // `FxEqualizerControl::restoreDefaults` (`FxAudioControls.cpp:529-544`): ten bands,
        // leveling, balance, width and gain back to their defaults — and the curve, which is the
        // preset's, carried onto the ten bands rather than flattened (U1).
        let mut app = with_presets(&["Jazz"]);
        app.handle(&[
            UiAction::SetBandCount(31),
            UiAction::SetBandGain(0, 9.0),
            UiAction::SetBandGain(30, -5.0),
            UiAction::SetMasterGain(-6.0),
            UiAction::SetVolumeLeveling(3.0),
            UiAction::SetBalance(4.0),
            UiAction::SetFilterQ(2.5),
        ]);
        let curve = gains(&app);
        app.handle(&[UiAction::RestoreDefaults]);

        assert_eq!(gains(&app), fxsound_dsp::eq::remap_band_gains(&curve, 10));
        assert_eq!(gains(&app)[0], 9.0);
        assert_eq!(gains(&app)[9], -5.0);
        assert_eq!(centres(&app), ladder(10));
        assert_eq!(app.settings.num_bands, 10);
        assert_eq!(app.params().num_bands, 10);
        assert_eq!(app.state.master_gain_db, 0.0);
        assert_eq!(app.state.volume_leveling, 0.0);
        assert_eq!(app.state.balance_db, 0.0);
        assert_eq!(app.state.filter_q, 1.0);
        assert_eq!(
            (
                app.settings.master_gain,
                app.settings.volume_leveling,
                app.settings.balance,
                app.settings.filter_q
            ),
            (0.0, 0.0, 0.0, 1.0),
            "the speakers' levels are settings over every preset"
        );
    }

    #[test]
    fn restoring_defaults_on_ten_bands_is_no_edit_to_the_speakers_preset() {
        // The levels are settings, not the preset's, and ten bands stay the curve they are.
        let mut app = with_presets(&["Jazz"]);
        app.handle(&[UiAction::SetVolumeLeveling(3.0)]);
        assert!(!app.state.presets[0].modified);
        app.handle(&[UiAction::RestoreDefaults]);
        assert!(!app.state.presets[0].modified);
        assert_eq!(app.state.volume_leveling, 0.0);
    }

    #[test]
    fn changing_the_band_count_carries_the_curve_over_by_position() {
        // Upstream 182a329 (`GraphicEqSet.cpp:200-245`): 10 → 31 used to wipe the curve flat.
        let mut app = with_presets(&["Jazz"]);
        app.handle(&[
            UiAction::SetBandGain(0, 8.0),
            UiAction::SetBandGain(4, 6.0),
            UiAction::SetBandGain(9, -4.0),
        ]);
        let ten = gains(&app);
        app.handle(&[UiAction::SetBandCount(31)]);

        assert_eq!(app.state.eq_bands.len(), 31);
        assert_eq!(gains(&app), fxsound_dsp::eq::remap_band_gains(&ten, 31));
        assert_eq!(gains(&app)[0], 8.0, "the first band lands on the first");
        assert_eq!(gains(&app)[30], -4.0, "and the last on the last");
        // The 31-band ISO ladder starts at 20 Hz and ends at 20 kHz.
        assert_eq!(app.state.eq_bands[0].center_hz, 20.0);
        assert_eq!(app.state.eq_bands[30].center_hz, 20000.0);
        assert_eq!(app.params().num_bands, 31);
        assert_eq!(app.params().bands().1, gains(&app).as_slice());
        assert_eq!(app.settings.num_bands, 31);
        assert!(app.state.presets[0].modified);

        // And back: the ends come home exactly, the rest close to where they were.
        app.handle(&[UiAction::SetBandCount(10)]);
        assert_eq!(gains(&app)[0], 8.0);
        assert_eq!(gains(&app)[9], -4.0);
        assert_eq!(centres(&app), ladder(10));
    }

    #[test]
    fn fewer_bands_keep_a_narrow_boost_at_its_height_rather_than_averaging_it() {
        // Shrinking picks the nearest band, `1 + (int)((i-1)*(old-1)/(new-1) + 0.5)`: from 31 to
        // 5 that is bands 1, 9, 16, 24 and 31, so a boost on band 16 alone survives whole.
        let mut app = headless();
        app.handle(&[UiAction::SetBandCount(31), UiAction::SetBandGain(15, 9.0)]);
        app.handle(&[UiAction::SetBandCount(5)]);
        assert_eq!(gains(&app), [0.0, 0.0, 9.0, 0.0, 0.0]);
    }

    #[test]
    fn a_microphone_changes_its_own_band_count_and_leaves_the_speakers_setting_alone() {
        // The band count Settings keeps is the music chain's, like the four level settings; a
        // voice preset carries its own ladder.
        let mut app = headless();
        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Input)]);
        app.handle(&[UiAction::SetBandCount(5)]);
        assert_eq!(app.input_params().num_bands, 5);
        assert_eq!(app.settings.num_bands, 10);
        assert_eq!(
            app.params().num_bands,
            10,
            "the speakers' chain is untouched"
        );
    }

    /// A start from [`saved_settings`] with the speakers on 31 bands and their four levels off
    /// their defaults, the window then moved to the microphone and its voice preset `voice`.
    fn restoring_on_the_microphone(voice: &str) -> (App, FakeEngine, tempfile::TempDir) {
        let mut settings = saved_settings(OUT);
        settings.input_preset = voice.to_owned();
        settings.num_bands = 31;
        settings.filter_q = 2.0;
        settings.balance = 3.0;
        settings.volume_leveling = 2.0;
        let (mut app, engine, dir) = started_with(settings);
        app.handle(&[UiAction::SetEditDirection(IN)]);
        assert_eq!(app.lane_preset(IN), Some((voice, false)));
        (app, engine, dir)
    }

    /// The speakers' side of the settings file: their band count and their four levels.
    fn speakers_settings(app: &App) -> (u32, f32, f32, f32, f32) {
        let s = &app.settings;
        (
            s.num_bands,
            s.master_gain,
            s.filter_q,
            s.balance,
            s.volume_leveling,
        )
    }

    #[test]
    fn restoring_defaults_on_a_microphone_carries_its_curve_onto_ten_bands_as_an_edit_to_it() {
        // On a voice the band count and the gain are the preset's own ([`App::set_band_count`],
        // `SetMasterGain`), so Restore Defaults edits the voice preset; the speakers' band count
        // and levels are settings over the music presets, which it does not reach.
        let (mut app, _engine, _dir) = restoring_on_the_microphone("Loud");
        let speakers = (speakers_settings(&app), *app.params());
        app.handle(&[
            UiAction::SetBandCount(5),
            UiAction::SetBandGain(0, 6.0),
            UiAction::SetBandGain(4, -4.0),
            UiAction::SetMasterGain(12.0),
            UiAction::SetFilterQ(2.5),
        ]);
        let curve = gains(&app);
        app.handle(&[UiAction::RestoreDefaults]);

        assert_eq!(app.input_params().num_bands, 10);
        assert_eq!(gains(&app), fxsound_dsp::eq::remap_band_gains(&curve, 10));
        assert_eq!(gains(&app)[0], 6.0);
        assert_eq!(gains(&app)[9], -4.0);
        assert_eq!(centres(&app), ladder(10));
        assert_eq!(app.input_params().makeup_db, 0.0);
        assert_eq!(app.input_params().filter_q, 1.0);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));
        assert_eq!(
            (speakers_settings(&app), *app.params()),
            speakers,
            "the speakers' band count, levels and chain are as they were"
        );
    }

    #[test]
    fn restoring_defaults_takes_a_voice_presets_makeup_to_zero_as_an_edit_to_it() {
        // `Loud` carries 9 dB of makeup and ten bands: the gain is the only thing that moves, and
        // it is the preset's.
        let (mut app, _engine, _dir) = restoring_on_the_microphone("Loud");
        let before = speakers_settings(&app);
        app.handle(&[UiAction::RestoreDefaults]);
        assert_eq!(app.input_params().makeup_db, 0.0);
        assert_eq!(app.input_params().num_bands, 10);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));
        assert_eq!(speakers_settings(&app), before);
    }

    #[test]
    fn a_voices_makeup_gain_keeps_the_odd_decibel_the_window_sets() {
        // On a microphone the column's second face steps the gain in whole decibels (U18), since
        // voice presets are voiced at 3, 5, 7 and 9: the controller keeps 7 as 7 rather than
        // putting it on the speakers' two-decibel grid, and it is an edit to the voice preset.
        let (mut app, _engine, _dir) = restoring_on_the_microphone("Loud");
        let before = speakers_settings(&app);
        app.handle(&[UiAction::SetMasterGain(7.0)]);
        assert_eq!(app.state.master_gain_db, 7.0);
        assert_eq!(app.input_params().makeup_db, 7.0);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));
        assert_eq!(speakers_settings(&app), before);
    }

    #[test]
    fn restoring_defaults_on_a_voice_with_only_its_width_moved_is_no_edit_to_it() {
        // `Quiet` has no makeup and ten bands; the filter width is not written into a voice
        // preset, so putting it back changes nothing a save would keep.
        let (mut app, _engine, _dir) = restoring_on_the_microphone("Quiet");
        let before = speakers_settings(&app);
        app.handle(&[UiAction::SetFilterQ(2.5)]);
        assert_eq!(app.lane_preset(IN), Some(("Quiet", false)));
        app.handle(&[UiAction::RestoreDefaults]);
        assert_eq!(app.input_params().filter_q, 1.0);
        assert_eq!(app.input_params().makeup_db, 0.0);
        assert_eq!(app.input_params().num_bands, 10);
        assert_eq!(app.lane_preset(IN), Some(("Quiet", false)));
        assert_eq!(speakers_settings(&app), before);
    }

    #[test]
    fn toggling_the_view_and_theme_records_the_choice_in_the_settings() {
        let mut app = headless();
        assert_eq!(app.state.view, ViewMode::Pro);
        app.handle(&[UiAction::ToggleView]);
        assert_eq!(app.state.view, ViewMode::Lite);
        assert_eq!(app.settings.view, ViewMode::Lite);

        assert_eq!(app.state.theme, ThemeMode::Dark);
        app.handle(&[UiAction::ToggleTheme]);
        assert_eq!(app.state.theme, ThemeMode::Light);
        assert_eq!(app.settings.theme_mode, ThemeMode::Light);
        assert!(app.palette().mode() == ThemeMode::Light);
    }

    #[test]
    fn the_interface_and_the_equalizer_agree_on_which_bands_the_device_can_carry() {
        // `UiState::band_is_live` decides whether to strike a frequency label through;
        // `GraphicEq::set_band_boost` decides whether the filter is built at all. They live in
        // different crates and the interface cannot see the equalizer, so the rule is written
        // twice — and this is what stops the two copies drifting into a label that says a band is
        // live while the engine is quietly bypassing it.
        //
        // The equalizer is asked the only question that matters: does a boost change the curve.
        use fxsound_dsp::GraphicEq;

        for rate in [8_000_u32, 16_000, 22_050, 32_000, 44_100, 48_000, 96_000] {
            let mut state = UiState {
                sample_rate: rate,
                ..UiState::default()
            };
            let mut eq = GraphicEq::new();
            eq.set_sample_rate(rate as f32);

            for band in 0..state.eq_bands.len() {
                let centre = state.eq_bands[band].center_hz;
                state.eq_bands[band].boost_db = 6.0;
                eq.set_band_boost(band, 6.0);
                let engine_built_it = eq.response_db(centre).abs() > 0.01;
                assert_eq!(
                    state.band_is_live(band),
                    engine_built_it,
                    "{rate} Hz, band {band} at {centre} Hz: the interface says {} and the \
                     equalizer says {engine_built_it}",
                    state.band_is_live(band),
                );
                state.eq_bands[band].boost_db = 0.0;
                eq.set_band_boost(band, 0.0);
            }
        }
    }

    /// Give `app` these voice presets, as the factory set of a voice store in a scratch directory
    /// the returned guard keeps alive: selecting one loads it from its file.
    fn use_voices(
        app: &mut App,
        voices: &[fxsound_preset::input::InputPreset],
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("scratch directory");
        app.voice_presets = voice_store_for_tests(
            voices,
            &dir.path().join("voice-factory"),
            dir.path().join("user").join("Input"),
        );
        dir
    }

    /// Two voice presets, so a crossing has somewhere to land.
    #[must_use]
    fn with_voice_presets(app: &mut App) -> tempfile::TempDir {
        use fxsound_preset::input::{Equalizer, InputPreset};
        let voice = |name: &str, hz: f32| InputPreset {
            name: name.to_owned(),
            description: String::new(),
            rnnoise: false,
            highpass_hz: hz,
            highpass_order: 2,
            gate: None,
            compressor: None,
            deesser: None,
            eq: Equalizer {
                centers_hz: fxsound_core::eq::DEFAULT_CENTERS_HZ.to_vec(),
                gains_db: vec![0.0; 10],
                enabled: true,
            },
            makeup_db: 0.0,
            ceiling_db: -3.0,
            ..InputPreset::default()
        };
        use_voices(app, &[voice("Clean Voice", 80.0), voice("Flat", 75.0)])
    }

    #[test]
    fn crossing_to_a_microphone_swaps_the_whole_preset_list() {
        // The two sets are never merged: a music preset on a voice is wrong by construction, and a
        // list holding both would make picking the wrong one a normal thing to do.
        let mut app = app_with_two_presets("directions");
        let _voices = with_voice_presets(&mut app);
        app.state.devices = vec![
            device("alsa_output.speakers", DeviceDirection::Output, true),
            device("alsa_input.mic", DeviceDirection::Input, false),
        ];
        app.settings.input_preset = "Flat".to_owned();

        app.handle(&[UiAction::SelectDevice(0), UiAction::SelectPreset(1)]);
        assert_eq!(app.settings.output_preset, "Beta");
        let names: Vec<&str> = app.state.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Beta"], "the speakers show the .fac set");

        app.handle(&[UiAction::SelectDevice(1)]);
        let names: Vec<&str> = app.state.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            ["Clean Voice", "Flat"],
            "the microphone shows the voice set"
        );
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Flat"));
        assert_eq!(
            app.settings.output_preset, "Beta",
            "the other direction is untouched"
        );

        // And back again, to the music set and what the speakers had.
        app.handle(&[UiAction::SelectDevice(0)]);
        let names: Vec<&str> = app.state.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Beta"]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Beta"));
    }

    /// §4 of the 0.4.0 design: `chain = "podcast"` is the one thing in a voice preset the
    /// parameter snapshot cannot carry, so it travels as a control message. What the app keeps is
    /// the name it last sent; the engine builds it ([`fxsound_dsp::ChainSpec::by_name`]), which
    /// is asserted here on the same chain the audio thread would build from that name.
    #[test]
    fn a_voice_preset_naming_a_chain_tells_the_audio_thread_which_one() {
        use fxsound_preset::input::{DEFAULT_CHAIN, InputPreset};
        let mut app = App::headless_for_tests();
        app.state.direction = DeviceDirection::Input;
        let _voices = use_voices(
            &mut app,
            &[
                InputPreset {
                    name: "Voice".to_owned(),
                    ..InputPreset::default()
                },
                InputPreset {
                    name: "Podcast".to_owned(),
                    chain: "podcast".to_owned(),
                    ..InputPreset::default()
                },
            ],
        );
        app.refresh_preset_list();
        assert_eq!(app.input_chain(), DEFAULT_CHAIN);
        // The store lists by name, so the list is Podcast, Voice.
        let at = |app: &App, name: &str| {
            app.state
                .presets
                .iter()
                .position(|p| p.name == name)
                .expect("listed")
        };

        app.handle(&[UiAction::SelectPreset(at(&app, "Podcast"))]);
        assert_eq!(app.input_chain(), "podcast");
        let spec =
            fxsound_dsp::ChainSpec::by_name(app.input_chain()).expect("a chain the engine builds");
        assert_eq!(spec, fxsound_dsp::ChainSpec::podcast());
        let engine = fxsound_dsp::InputEngine::new_with_spec(48_000.0, 512, 1, spec);
        assert_eq!(engine.spec(), fxsound_dsp::ChainSpec::podcast());
        assert!(
            engine.chain().gate().is_none(),
            "the podcast ordering has no gate"
        );

        // Back to a preset that names none: the default chain, said again.
        app.handle(&[UiAction::SelectPreset(at(&app, "Voice"))]);
        assert_eq!(app.input_chain(), DEFAULT_CHAIN);
    }

    #[test]
    fn a_voice_preset_moves_the_stages_the_interface_has_no_control_for() {
        // The whole reason the voice set is navigated by preset: the gate, the compressor and the
        // de-esser have no knobs anywhere, so if a preset does not move them nothing does.
        use fxsound_preset::input::{Compressor, Equalizer, Gate, InputPreset};
        let mut app = App::headless_for_tests();
        app.state.direction = DeviceDirection::Input;
        let _voices = use_voices(
            &mut app,
            &[InputPreset {
                name: "Voiced".to_owned(),
                description: String::new(),
                rnnoise: true,
                highpass_hz: 90.0,
                highpass_order: 4,
                gate: Some(Gate {
                    threshold_db: -40.0,
                    ratio: 2.0,
                    range_db: -12.0,
                    attack_ms: 5.0,
                    release_ms: 150.0,
                    hold_ms: 200.0,
                    detection: fxsound_core::Detection::Rms,
                }),
                compressor: Some(Compressor {
                    threshold_db: -20.0,
                    ratio: 4.0,
                    knee_db: 6.0,
                    attack_ms: 20.0,
                    release_ms: 150.0,
                    detection: fxsound_core::Detection::Rms,
                }),
                deesser: None,
                eq: Equalizer {
                    centers_hz: fxsound_core::eq::DEFAULT_CENTERS_HZ.to_vec(),
                    gains_db: vec![0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.5, 0.0, 0.0, 0.0],
                    enabled: true,
                },
                makeup_db: 4.0,
                ceiling_db: -3.0,
                ..InputPreset::default()
            }],
        );
        app.refresh_preset_list();
        app.handle(&[UiAction::SelectPreset(0)]);

        assert!(app.state.denoise_on);
        assert!(app.state.gate_on);
        assert!(app.state.compressor_on);
        assert!(!app.state.deesser_on, "an absent table is an absent stage");

        let published = app.input_params();
        assert_eq!(published.highpass_hz, 90.0);
        assert_eq!(published.highpass_order, 4);
        assert_eq!(published.gate_threshold_db, -40.0);
        assert_eq!(published.compressor_ratio, 4.0);
        assert_eq!(published.makeup_db, 4.0);
        assert!(published.rnnoise);
        assert_eq!(published.band_boost_db[6], 1.5);
    }

    #[test]
    fn switching_between_two_speakers_never_moves_the_preset() {
        // The `crossed` guard exists for this, and the mutation run showed why it needs a test of
        // its own: without the guard, every device switch would reapply whatever the settings file
        // records for the direction. That is a no-op only for as long as the file and the
        // selection agree, and they are two pieces of state kept in step by hand. Here they are
        // deliberately out of step — which is the shape of any future bug that separates them —
        // and a same-direction switch must still leave the user's preset alone.
        let mut app = app_with_two_presets("same-direction");
        app.handle(&[UiAction::SelectDevice(0), UiAction::SelectPreset(1)]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Beta"));

        app.settings.output_preset = "Alpha".to_owned();
        app.handle(&[UiAction::SelectDevice(1)]);
        assert_eq!(
            app.state.preset().map(|p| p.name.as_str()),
            Some("Beta"),
            "picking another speaker changed the preset"
        );
    }

    #[test]
    fn hiding_with_no_tray_says_so_instead_of_pointing_at_an_icon() {
        // The GNOME-without-AppIndicator case: no window, no icon, and until this existed nothing
        // said about either — a running process the user can neither see nor reach. "Click
        // FxSound icon to reopen" is not advice on a session with no icon.
        let with_tray = Message::minimised_to_tray().body;
        let without = Message::hidden_with_no_tray().body;
        assert!(with_tray.contains("Click FxSound icon"), "{with_tray}");
        assert!(without.contains("--show"), "{without}");
        assert!(!without.contains("Click FxSound icon"), "{without}");

        // And the tip is shown once per process either way, as the original does.
        let mut app = App::headless_for_tests();
        assert!(!app.tray_tip_shown);
        app.notify_hidden_to_tray(false);
        assert!(app.tray_tip_shown);
        app.notify_hidden_to_tray(true);
        assert!(app.tray_tip_shown, "the second call must be a no-op");
    }

    #[test]
    fn an_output_command_before_the_device_list_waits_for_it() {
        // The bug this closes: the control socket answers as soon as the GUI thread is up, which
        // is before PipeWire has finished enumerating, so `fxsound --output "..."` at login
        // returned 0, printed nothing and did nothing. Anything scripted hits it.
        let mut app = App::headless_for_tests();
        assert!(!app.has_seen_devices());

        let outcome = crate::commands::run(
            &mut app,
            &[crate::cli::Command::Output(
                crate::cli::OutputCommand::Select("alsa_input.mic".to_owned()),
            )],
        );
        assert!(
            !outcome.failed,
            "a command that is about to become valid must not fail"
        );
        assert!(outcome.stderr.is_empty());
        assert_eq!(app.state.selected_device(), None, "nothing to select yet");

        // The list arrives, and the command it was waiting for happens.
        app.devices_seen = true;
        app.state.devices = vec![
            device("alsa_output.speakers", DeviceDirection::Output, true),
            device("alsa_input.mic", DeviceDirection::Input, false),
        ];
        app.apply_pending_device();
        assert_eq!(app.state.selected_device(), Some(1));
        assert_eq!(app.state.direction, DeviceDirection::Input);
    }

    #[test]
    fn an_output_command_for_a_device_that_does_not_exist_says_so_and_fails() {
        // The other half. Once the list exists, a name that matches nothing in it is a name that
        // is not a device, and a script has to be able to find that out.
        let mut app = App::headless_for_tests();
        app.devices_seen = true;
        app.state.devices = vec![device(
            "alsa_output.speakers",
            DeviceDirection::Output,
            true,
        )];

        let outcome = crate::commands::run(
            &mut app,
            &[crate::cli::Command::Output(
                crate::cli::OutputCommand::Select("nothing like this".to_owned()),
            )],
        );
        assert!(outcome.failed, "it exited zero while doing nothing");
        assert!(
            outcome.stderr.contains("nothing like this"),
            "the message should name what was asked for: {:?}",
            outcome.stderr
        );
        assert_eq!(app.state.selected_device(), None);
    }

    #[test]
    fn a_name_that_is_not_a_device_does_not_wait_for_the_next_list() {
        // A typo held for the next device list would change the device half a minute later, when
        // something unrelated is plugged in. Once a list has been seen, the pending name is spent.
        let mut app = App::headless_for_tests();
        app.select_device_when_listed("typo", DeviceDirection::Output);
        app.devices_seen = true;
        app.state.devices = vec![device(
            "alsa_output.speakers",
            DeviceDirection::Output,
            true,
        )];
        app.apply_pending_device();
        assert_eq!(app.state.selected_device(), None);

        app.state
            .devices
            .push(device("typo", DeviceDirection::Output, false));
        app.apply_pending_device();
        assert_eq!(
            app.state.selected_device(),
            None,
            "a spent name came back to life"
        );
    }

    #[test]
    fn a_microphone_with_no_voice_presets_installed_still_runs() {
        // A build without `assets/presets/Input` is a build whose microphone chain runs on its
        // defaults, which is a working chain: an 80 Hz high-pass and every dynamics stage off.
        // The picker is empty, nothing is selected, and nothing pretends otherwise.
        let mut app = app_with_two_presets("no-voice");
        app.state.devices = vec![
            device("alsa_output.speakers", DeviceDirection::Output, true),
            device("alsa_input.mic", DeviceDirection::Input, false),
        ];
        app.handle(&[UiAction::SelectDevice(0), UiAction::SelectPreset(1)]);
        app.handle(&[UiAction::SelectDevice(1)]);

        assert!(
            app.state.presets.is_empty(),
            "there are no voice presets to show"
        );
        assert_eq!(app.state.preset(), None);
        let published = app.input_params();
        assert_eq!(published.highpass_hz, 80.0);
        assert_eq!(published.highpass_order, 2);
        assert!(!published.gate_on && !published.compressor_on && !published.deesser_on);
        assert!(!published.rnnoise);
    }

    #[test]
    fn picking_a_microphone_switches_the_interface_in_the_same_frame() {
        // Not on the next device list: picking a microphone is the moment the five effect sliders
        // stop meaning anything, and a frame of them still looking live is a frame of lying.
        let mut app = App::headless_for_tests();
        app.state.devices = vec![
            device("speakers", DeviceDirection::Output, true),
            device("mic", DeviceDirection::Input, false),
        ];
        assert_eq!(app.state.direction, DeviceDirection::Output);
        app.handle(&[UiAction::SelectDevice(1)]);
        assert_eq!(app.state.direction, DeviceDirection::Input);
        assert!(!app.state.music_effects_apply());

        app.handle(&[UiAction::SelectDevice(0)]);
        assert_eq!(app.state.direction, DeviceDirection::Output);
        assert!(app.state.music_effects_apply());
    }

    #[test]
    fn the_controls_a_voice_shares_with_music_reach_the_microphone_chain() {
        // While the microphone is the lane being edited; the next test is the other half.
        let mut app = App::headless_for_tests();
        app.state.direction = DeviceDirection::Input;
        app.state.power = true;
        app.state.eq_on = true;
        app.state.master_gain_db = -4.0;
        app.state.eq_bands[2].boost_db = 3.5;
        app.sync_params_from_state();

        let input = app.input_params();
        assert!(input.power);
        assert!(input.eq_on);
        assert_eq!(
            input.makeup_db, -4.0,
            "the gain slider is the chain's makeup"
        );
        let (_, boosts) = input.bands();
        assert_eq!(boosts[2], 3.5, "the equalizer is what the two chains share");
    }

    #[test]
    fn editing_the_output_never_reaches_the_microphone_snapshot() {
        // The other half of the test above: a music preset's equalizer and gain on a voice is the
        // cross-direction leak the 0.3.0 audit found.
        let mut app = App::headless_for_tests();
        let before = *app.input_params();
        app.handle(&[
            UiAction::SetMasterGain(8.0),
            UiAction::SetBandGain(2, 6.0),
            UiAction::SetFilterQ(2.5),
        ]);
        assert_eq!(app.params().master_gain_db, 8.0);
        assert_eq!(*app.input_params(), before, "the microphone chain moved");
    }

    #[test]
    fn the_microphone_starts_unvoiced_whatever_the_speakers_are_doing() {
        let app = App::headless_for_tests();
        let input = app.input_params();
        assert_eq!(input.makeup_db, 0.0);
        assert!(input.bands().1.iter().all(|&gain| gain == 0.0));
        assert!(!input.rnnoise);
    }

    /// Speakers, headphones and a microphone.
    fn lanes() -> App {
        let mut app = headless();
        app.state.devices = vec![
            device("alsa_output.speakers", DeviceDirection::Output, true),
            device("alsa_output.headphones", DeviceDirection::Output, false),
            device("alsa_input.mic", DeviceDirection::Input, false),
        ];
        app
    }

    #[test]
    fn picking_a_microphone_leaves_the_speakers_selected() {
        let mut app = lanes();
        app.handle(&[UiAction::SelectOutput(1), UiAction::SelectInput(2)]);
        assert_eq!(
            app.state.selected_output,
            Some(1),
            "the output lane is still there"
        );
        assert_eq!(app.state.selected_input, Some(2));
        assert_eq!(app.state.direction, DeviceDirection::Input);
        assert!(app.settings.lane_enabled(DeviceDirection::Output));
        assert!(app.settings.lane_enabled(DeviceDirection::Input));
        assert_eq!(
            app.settings.device_name(DeviceDirection::Output),
            "alsa_output.headphones"
        );
        assert_eq!(
            app.settings.device_name(DeviceDirection::Input),
            "alsa_input.mic"
        );
    }

    #[test]
    fn select_device_resolves_to_the_lane_of_the_device_it_names() {
        let mut app = lanes();
        app.handle(&[UiAction::SelectDevice(0)]);
        assert_eq!(app.state.selected_output, Some(0));
        app.handle(&[UiAction::SelectDevice(2)]);
        assert_eq!(app.state.selected_input, Some(2));
        assert_eq!(
            app.state.selected_output,
            Some(0),
            "a microphone is not a speaker"
        );
        assert_eq!(
            app.state.selected_device(),
            Some(2),
            "and it is now being edited"
        );
    }

    #[test]
    fn a_device_offered_to_the_wrong_lane_is_refused() {
        let mut app = lanes();
        app.handle(&[UiAction::SelectOutput(2), UiAction::SelectInput(0)]);
        assert_eq!(app.state.selected_output, None);
        assert_eq!(app.state.selected_input, None);
        assert!(app.settings.device_name(DeviceDirection::Output).is_empty());
        assert!(app.settings.device_name(DeviceDirection::Input).is_empty());
        assert_eq!(app.state.direction, DeviceDirection::Output);
    }

    #[test]
    fn detaching_the_microphone_leaves_the_speakers_alone_and_is_remembered() {
        let mut app = lanes();
        app.handle(&[UiAction::SelectOutput(0), UiAction::SelectInput(2)]);
        app.state.input_active = true;
        app.handle(&[UiAction::DetachInput]);
        assert_eq!(app.state.selected_input, None);
        assert!(!app.state.input_active);
        assert_eq!(app.state.selected_output, Some(0));
        assert!(!app.settings.lane_enabled(DeviceDirection::Input));
        assert!(app.settings.lane_enabled(DeviceDirection::Output));
        // The device is remembered for the next time the lane is turned on.
        assert_eq!(
            app.settings.device_name(DeviceDirection::Input),
            "alsa_input.mic"
        );
    }

    #[test]
    fn detaching_the_speakers_leaves_the_microphone_alone() {
        let mut app = lanes();
        app.handle(&[UiAction::SelectInput(2), UiAction::SelectOutput(1)]);
        app.handle(&[UiAction::DetachOutput]);
        assert_eq!(app.state.selected_output, None);
        assert_eq!(app.state.selected_input, Some(2));
        assert!(!app.settings.lane_enabled(DeviceDirection::Output));
    }

    #[test]
    fn a_lane_shows_what_the_engine_attached_it_to_and_never_a_guess_from_the_list() {
        let devices = vec![
            device("alsa_output.speakers", DeviceDirection::Output, true),
            device("alsa_input.mic", DeviceDirection::Input, true),
            device("alsa_input.webcam", DeviceDirection::Input, false),
        ];
        let mut settings = Settings::default();
        settings.set_device_name(DeviceDirection::Input, "alsa_input.webcam");
        settings.set_lane_enabled(DeviceDirection::Input, true);
        let input = DeviceDirection::Input;
        assert_eq!(
            lane_selection(&settings, &devices, DeviceDirection::Output, None, None),
            None,
            "attached to nothing: not the server's default, which FxSound is not in front of"
        );
        assert_eq!(
            lane_selection(&settings, &devices, input, None, None),
            None,
            "nor the saved microphone, which nothing has attached"
        );
        assert_eq!(
            lane_selection(&settings, &devices, input, Some("alsa_input.webcam"), None),
            Some(2)
        );
        settings.set_lane_enabled(DeviceDirection::Input, false);
        assert_eq!(
            lane_selection(&settings, &devices, input, Some("alsa_input.webcam"), None),
            None,
            "a lane switched off shows Off whatever the engine last said"
        );
    }

    #[test]
    fn a_request_is_shown_until_the_engine_answers_it_whatever_the_lane_is_attached_to() {
        let devices = vec![
            device("alsa_output.speakers", DeviceDirection::Output, true),
            device("bluez_output.headphones", DeviceDirection::Output, false),
            device("alsa_input.mic", DeviceDirection::Input, true),
        ];
        let settings = Settings::default();
        let output = DeviceDirection::Output;
        assert_eq!(
            lane_selection(
                &settings,
                &devices,
                output,
                None,
                Some("bluez_output.headphones")
            ),
            Some(1),
            "asked for and not answered yet"
        );
        // Still on the speakers while the engine has not got to it: the pick is where the lane is
        // going, and the lane's answer — an `Attached` or an error — is what ends the request.
        assert_eq!(
            lane_selection(
                &settings,
                &devices,
                output,
                Some("alsa_output.speakers"),
                Some("bluez_output.headphones")
            ),
            Some(1),
            "asked for while attached elsewhere"
        );
        // Answered: the attachment is all there is.
        assert_eq!(
            lane_selection(
                &settings,
                &devices,
                output,
                Some("alsa_output.speakers"),
                None
            ),
            Some(0)
        );
        // A request for a device that has gone shows where the lane still is.
        assert_eq!(
            lane_selection(
                &settings,
                &devices,
                output,
                Some("alsa_output.speakers"),
                Some("gone")
            ),
            Some(0)
        );
        // A name the list does not carry in the lane's direction is not shown, whichever it is.
        for stale in ["gone", "alsa_input.mic"] {
            assert_eq!(
                lane_selection(&settings, &devices, output, Some(stale), None),
                None,
                "attached {stale}"
            );
            assert_eq!(
                lane_selection(&settings, &devices, output, None, Some(stale)),
                None,
                "asked for {stale}"
            );
        }
    }

    #[test]
    fn an_attachment_moves_the_combo_and_a_rescan_keeps_it_there() {
        let mut app = lanes();
        app.settings
            .set_device_name(DeviceDirection::Input, "alsa_input.mic");
        app.settings.set_lane_enabled(DeviceDirection::Input, true);
        app.receive(AudioToUi::Attached {
            direction: DeviceDirection::Output,
            node_name: Some("alsa_output.speakers".to_owned()),
        });
        app.receive(AudioToUi::Devices(app.state.devices.clone()));
        assert_eq!(app.state.selected_output, Some(0));
        assert_eq!(
            app.state.selected_input,
            Some(2),
            "the saved microphone, announced and not answered yet"
        );

        app.receive(AudioToUi::Attached {
            direction: DeviceDirection::Output,
            node_name: Some("alsa_output.headphones".to_owned()),
        });
        assert_eq!(app.state.selected_output, Some(1));
        assert_eq!(
            app.attached(DeviceDirection::Output),
            Some("alsa_output.headphones")
        );

        // A rescan keeps showing where the lane is, not the saved choice it moved away from.
        app.adopt_device_list(app.state.devices.clone());
        assert_eq!(app.state.selected_output, Some(1));
        assert_eq!(
            app.state.selected_input,
            Some(2),
            "the other lane is its own"
        );
    }

    #[test]
    fn switching_the_edit_direction_swaps_the_preset_list_and_back() {
        let mut app = app_with_two_presets("edit-direction");
        let _voices = with_voice_presets(&mut app);
        app.settings.input_preset = "Flat".to_owned();
        app.handle(&[UiAction::SelectPreset(1)]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Beta"));

        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Input)]);
        assert_eq!(app.state.direction, DeviceDirection::Input);
        assert_eq!(app.settings.device_direction, DeviceDirection::Input);
        let names: Vec<&str> = app.state.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Clean Voice", "Flat"]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Flat"));

        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Output)]);
        let names: Vec<&str> = app.state.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Beta"]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Beta"));
        assert_eq!(app.settings.output_preset, "Beta");
        assert_eq!(app.settings.input_preset, "Flat");
    }

    #[test]
    fn switching_the_edit_direction_keeps_unsaved_edits_on_both_lanes() {
        let mut app = app_with_two_presets("edit-direction-edits");
        let _voices = with_voice_presets(&mut app);
        app.handle(&[UiAction::SelectPreset(0)]);
        app.handle(&[UiAction::SetEffect(Effect::Bass, 7.0)]);

        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Input)]);
        app.handle(&[UiAction::SetBandGain(4, 5.0)]);
        assert_eq!(app.input_params().bands().1[4], 5.0);

        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Output)]);
        assert_eq!(
            app.state.effect(Effect::Bass),
            7.0,
            "the music edit came back"
        );
        assert!(
            app.state.preset().is_some_and(|p| p.modified),
            "still marked unsaved"
        );
        assert_eq!(
            app.input_params().bands().1[4],
            5.0,
            "the voice edit kept running"
        );

        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Input)]);
        assert_eq!(app.state.eq_bands[4].boost_db, 5.0, "and it is shown again");
        assert!(!app.state.music_effects_apply());
    }

    #[test]
    fn switching_the_edit_direction_changes_nothing_the_engine_runs() {
        let mut app = app_with_two_presets("edit-direction-engine");
        let _voices = with_voice_presets(&mut app);
        app.state
            .devices
            .push(device("alsa_input.mic", DeviceDirection::Input, false));
        app.handle(&[UiAction::SelectOutput(0), UiAction::SelectInput(2)]);
        app.handle(&[UiAction::SetBandGain(1, 3.0)]);
        let (output, input) = (*app.params(), *app.input_params());
        let devices = (app.state.selected_output, app.state.selected_input);

        for direction in [
            DeviceDirection::Output,
            DeviceDirection::Input,
            DeviceDirection::Output,
            DeviceDirection::Input,
        ] {
            app.handle(&[UiAction::SetEditDirection(direction)]);
            assert_eq!(
                *app.params(),
                output,
                "{direction:?}: the music chain moved"
            );
            assert_eq!(
                *app.input_params(),
                input,
                "{direction:?}: the voice chain moved"
            );
            assert_eq!(
                (app.state.selected_output, app.state.selected_input),
                devices,
                "{direction:?}: a device moved"
            );
        }
    }

    #[test]
    fn a_first_look_at_a_microphone_with_no_presets_shows_its_own_chain() {
        let mut app = lanes();
        app.handle(&[UiAction::SetBandGain(3, 6.0), UiAction::SetMasterGain(10.0)]);
        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Input)]);
        assert!(app.state.presets.is_empty());
        assert_eq!(
            app.state.eq_bands[3].boost_db, 0.0,
            "not the music's equalizer"
        );
        assert_eq!(app.state.master_gain_db, 0.0, "not the music's gain");
    }

    #[test]
    fn the_level_settings_belong_to_the_music_chain() {
        let mut app = lanes();
        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Input)]);
        app.handle(&[
            UiAction::SetMasterGain(9.0),
            UiAction::SetFilterQ(3.0),
            UiAction::SetBalance(4.0),
            UiAction::SetVolumeLeveling(2.0),
        ]);
        assert_eq!(app.settings.master_gain, 0.0);
        assert_eq!(app.settings.filter_q, 1.0);
        assert_eq!(app.settings.balance, 0.0);
        assert_eq!(app.settings.volume_leveling, 0.0);
        assert_eq!(
            app.input_params().makeup_db,
            9.0,
            "it is the voice's makeup"
        );
    }

    #[test]
    fn setting_the_same_edit_direction_again_is_a_no_op() {
        let mut app = app_with_two_presets("edit-direction-same");
        app.handle(&[UiAction::SelectPreset(1)]);
        let before = app.state.clone();
        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Output)]);
        assert_eq!(app.state.selected_preset, before.selected_preset);
        assert_eq!(app.state.presets, before.presets);
        assert!(
            app.lane_controls.iter().all(Option::is_none),
            "no lane was stored away"
        );
    }

    #[test]
    fn a_notice_can_be_dismissed_and_otherwise_expires_after_four_seconds() {
        let mut app = headless();
        app.state.notification = Some("Preset: Rock".to_owned());
        app.handle(&[UiAction::DismissNotice]);
        assert!(app.state.notification.is_none());

        app.state.notification = Some("Preset: Jazz".to_owned());
        app.poll_audio();
        assert!(app.state.notification.is_some(), "just stamped");
        let long_ago = Instant::now()
            .checked_sub(std::time::Duration::from_secs(5))
            .expect("the clock has run for five seconds");
        app.state.notice_clock = Some(("Preset: Jazz".to_owned(), long_ago));
        app.poll_audio();
        assert!(app.state.notification.is_none(), "four seconds are up");
    }

    #[test]
    fn the_strip_learns_what_the_voice_snapshot_asks_for() {
        // The level and the de-reverb are what the *preset* asked for, underneath the Settings
        // pane's overrides (which here all follow the preset), so that is where they are set.
        let mut app = headless();
        app.input_voicing.dereverb = fxsound_core::DereverbLevel::Medium;
        app.input_voicing.denoise_level = fxsound_core::DenoiseLevel::Strong;
        app.input_params.deesser_hz = 6_000.0;
        app.settings.echo_cancel = true;
        app.sync_params_from_state();
        assert!(app.state.dereverb_on);
        assert_eq!(app.state.denoise_level, fxsound_core::DenoiseLevel::Strong);
        assert_eq!(app.state.deesser_requested_hz, 6_000.0);
        assert!(app.state.echo_cancel_on);
    }

    #[test]
    fn the_voice_chains_dynamics_stay_off_until_a_preset_turns_them_on() {
        // Upgrading must not silently start gating someone's quiet talker, or compressing a voice
        // to numbers nobody has listened to. The high-pass is the exception and the comment on
        // `sync_input_params_from_state` says why.
        let mut app = App::headless_for_tests();
        app.sync_params_from_state();
        let input = app.input_params();
        assert!(!input.gate_on);
        assert!(!input.compressor_on);
        assert!(!input.deesser_on);
        assert_eq!(input.highpass_order, 2);
        assert_eq!(input.highpass_hz, 80.0);
    }

    #[test]
    fn cycling_presets_wraps_in_both_directions() {
        let mut app = with_presets(&["A", "B", "C"]);
        // Without a store on disk the load fails, but the index arithmetic is what matters here.
        assert_eq!(app.state.next_preset(), Some(1));
        app.state.selected_preset = Some(2);
        assert_eq!(app.state.next_preset(), Some(0));
        assert_eq!(app.state.previous_preset(), Some(1));
    }

    #[test]
    fn a_factory_preset_refuses_to_be_deleted() {
        let mut app = with_presets(&["Jazz"]);
        app.handle(&[UiAction::DeletePreset]);
        assert_eq!(app.state.presets.len(), 1);
        assert!(app.state.notification.is_some());
    }

    #[test]
    fn the_same_refusal_clicked_again_stays_up_four_seconds_from_the_second_click() {
        let mut app = with_presets(&["Jazz"]);
        app.handle(&[UiAction::DeletePreset]);
        let text = app.state.notification.clone().expect("the first refusal");
        // The first click was three and a half seconds ago.
        let first = Instant::now()
            .checked_sub(std::time::Duration::from_millis(3_500))
            .expect("the clock has run for a few seconds");
        app.state.notice_clock = Some((text.clone(), first));

        app.handle(&[UiAction::DeletePreset]);
        assert_eq!(app.state.notification.as_deref(), Some(text.as_str()));
        // Where the first click's four seconds run out, half a second after the second click.
        assert!(
            !app.state
                .expire_notification(first + std::time::Duration::from_millis(4_000)),
            "the second click kept the first click's clock"
        );
        assert!(app.state.notification.is_some());
    }

    #[test]
    fn a_rename_refusal_raised_again_restarts_its_clock_too() {
        let mut app = with_presets(&["Jazz"]);
        app.rename_preset("Blues");
        let long_ago = Instant::now()
            .checked_sub(std::time::Duration::from_secs(3))
            .expect("the clock has run for a few seconds");
        let text = app.state.notification.clone().expect("the refusal");
        app.state.notice_clock = Some((text, long_ago));
        app.rename_preset("Blues");
        let (_, since) = app.state.notice_clock.clone().expect("a clock");
        assert!(since > long_ago, "the clock was not restarted");
    }

    #[test]
    fn every_notice_the_application_raises_goes_through_its_clock() {
        // `UiState::notify` restarts the clock; a bare assignment of a text that is already up
        // keeps the old one, and the notice vanishes early. Tests may still write the field.
        let bare = concat!(".state.notification", " = ");
        for (file, source) in [
            ("app.rs", include_str!("app.rs")),
            ("main.rs", include_str!("main.rs")),
        ] {
            for (number, line) in source.lines().enumerate() {
                assert!(
                    !(line.trim_start().starts_with("self") && line.contains(bare)),
                    "{file}:{}: a notice written around UiState::notify",
                    number + 1
                );
            }
        }
    }

    #[test]
    fn every_notice_the_application_raises_reaches_the_event_stream() {
        // `App::raise_notice` is the one caller of `UiState::notify` outside the tests: a notice
        // raised around it is drawn but never reaches `--watch` or the D-Bus `Notice` signal.
        let direct = concat!(".state", ".notify(");
        let mut callers = Vec::new();
        for (file, source) in [
            ("app.rs", include_str!("app.rs")),
            ("main.rs", include_str!("main.rs")),
            ("commands.rs", include_str!("commands.rs")),
            ("dbus.rs", include_str!("dbus.rs")),
            ("ipc.rs", include_str!("ipc.rs")),
            ("tray.rs", include_str!("tray.rs")),
        ] {
            // rustfmt breaks a long call over lines; the guard reads it whole.
            let joined: String = source.split_whitespace().collect();
            callers.extend(std::iter::repeat_n(file, joined.matches(direct).count()));
        }
        assert_eq!(
            callers,
            ["app.rs"],
            "only App::raise_notice calls UiState::notify; raise a notice through it"
        );
    }

    /// A restart: a music store holding a flat `Alpha` and a bass-heavy `Beta`, the two voice
    /// presets, `Beta` and `Flat` saved as the two lanes' presets, and the window last on `edit`.
    fn restarted(tag: &str, edit: DeviceDirection) -> App {
        restarted_with(tag, edit, ("Beta", "Flat"))
    }

    /// [`restarted`] with the two saved names, music first.
    fn restarted_with(tag: &str, edit: DeviceDirection, saved: (&str, &str)) -> App {
        use fxsound_core::Preset;

        let dir =
            std::env::temp_dir().join(format!("fxsound-restart-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the preset directory");
        for (name, bass) in [("Alpha", 0.0), ("Beta", 0.6)] {
            let mut preset = Preset {
                name: name.to_owned(),
                ..Preset::default()
            };
            preset.set_effect(Effect::Bass, bass);
            fxsound_preset::save(&preset, &dir.join(format!("{name}.fac"))).expect("write");
        }

        let mut app = App::headless_for_tests();
        app.presets = fxsound_preset::PresetStore::with_dirs(
            vec![dir],
            std::env::temp_dir().join(format!("fxsound-restart-user-{}-{tag}", std::process::id())),
        );
        app.presets.rescan();
        let _voices = with_voice_presets(&mut app);
        app.settings.device_direction = edit;
        saved.0.clone_into(&mut app.settings.output_preset);
        saved.1.clone_into(&mut app.settings.input_preset);
        app.adopt_saved_presets();
        app
    }

    fn bass_of(app: &App) -> f32 {
        app.params().effect(Effect::Bass)
    }

    #[test]
    fn a_restart_editing_the_microphone_still_loads_the_speakers_saved_preset() {
        // The engine runs the speakers whenever their lane is on, whatever the window last edited:
        // loading only the edit direction's preset left them flat after this restart.
        let app = restarted("edit-input", DeviceDirection::Input);
        assert!(
            (bass_of(&app) - 0.6).abs() < 0.02,
            "the speakers run {} of bass, not Beta's",
            bass_of(&app)
        );
        assert_eq!(app.state.direction, DeviceDirection::Input);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Flat"));
        assert_eq!(
            app.input_params().highpass_hz,
            75.0,
            "the microphone runs Flat"
        );
    }

    #[test]
    fn a_restart_editing_the_speakers_still_loads_the_microphones_saved_preset() {
        // The engine runs the microphone beside the speakers whenever its lane is on, so it has to
        // be running its own preset from the start, not the unvoiced chain.
        let app = restarted("edit-output", DeviceDirection::Output);
        assert_eq!(
            app.input_params().highpass_hz,
            75.0,
            "the microphone runs Flat"
        );
        assert_eq!(app.state.direction, DeviceDirection::Output);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Beta"));
        assert!((bass_of(&app) - 0.6).abs() < 0.02);
    }

    #[test]
    fn the_lane_loaded_off_screen_at_start_comes_back_with_its_preset_selected() {
        let mut app = restarted("edit-input-then-output", DeviceDirection::Input);
        let (output, input) = (*app.params(), *app.input_params());
        app.handle(&[UiAction::SetEditDirection(DeviceDirection::Output)]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Beta"));
        assert!(
            app.state.effect(Effect::Bass) > 0.0,
            "the sliders show Beta"
        );
        assert_eq!(*app.params(), output, "looking at the speakers moved them");
        assert_eq!(
            *app.input_params(),
            input,
            "looking away moved the microphone"
        );
    }

    #[test]
    fn a_restart_records_each_lanes_preset_under_its_own_key() {
        let app = restarted_with("keys", DeviceDirection::Input, ("Gone", "Flat"));
        // A saved music preset that no longer exists falls back to the first, under the music's
        // key; loading the speakers first wrote nothing into the microphone's, and the window
        // still edits the microphone.
        let first = app.presets.entries()[0].name.clone();
        assert_eq!(app.settings.output_preset, first);
        assert_eq!(app.settings.input_preset, "Flat");
        assert_eq!(app.settings.device_direction, DeviceDirection::Input);
    }

    #[test]
    fn a_restart_on_a_microphone_without_voice_presets_leaks_no_music_level_into_it() {
        let mut app = App::headless_for_tests();
        app.settings.device_direction = DeviceDirection::Input;
        app.settings.filter_q = 3.0;
        app.settings.master_gain = 9.0;
        app.adopt_saved_presets();
        assert_eq!(app.state.direction, DeviceDirection::Input);
        let unvoiced = unvoiced_input_params();
        assert_eq!(app.input_params().makeup_db, unvoiced.makeup_db);
        assert_eq!(app.input_params().filter_q, unvoiced.filter_q);
        assert_eq!(
            app.state.master_gain_db, unvoiced.makeup_db,
            "the window shows the voice"
        );
        assert_eq!(
            app.params().master_gain_db,
            9.0,
            "the music keeps its own gain"
        );
        assert_eq!(app.params().filter_q, 3.0);
    }

    #[test]
    fn a_restart_on_the_speakers_leaves_a_microphone_without_voice_presets_unvoiced() {
        let mut app = App::headless_for_tests();
        app.settings.master_gain = 9.0;
        app.settings.filter_q = 3.0;
        app.adopt_saved_presets();
        assert_eq!(app.state.direction, DeviceDirection::Output);
        let unvoiced = unvoiced_input_params();
        assert_eq!(app.input_params().makeup_db, unvoiced.makeup_db);
        assert_eq!(app.input_params().filter_q, unvoiced.filter_q);
        assert!(app.input_params().bands().1.iter().all(|&gain| gain == 0.0));
        assert_eq!(app.state.master_gain_db, 9.0);
        assert_eq!(app.params().master_gain_db, 9.0);
    }

    #[test]
    fn window_level_actions_are_accepted_without_touching_the_dsp() {
        let mut app = headless();
        let before = *app.params();
        app.handle(&[
            UiAction::OpenSettings,
            UiAction::OpenMenu,
            UiAction::Minimise,
            UiAction::DragWindow,
        ]);
        assert_eq!(*app.params(), before);
    }

    #[test]
    fn the_eq_toggle_reaches_the_snapshot() {
        let mut app = headless();
        assert!(app.params().eq_on);
        app.handle(&[UiAction::SetEqEnabled(false)]);
        assert!(!app.params().eq_on);
    }

    #[test]
    fn selecting_a_device_that_does_not_exist_is_ignored() {
        let mut app = headless();
        app.handle(&[UiAction::SelectDevice(7)]);
        assert!(app.state.selected_device().is_none());
        assert!(app.settings.output_device_name.is_empty());
    }

    fn device(name: &str, direction: DeviceDirection, is_default: bool) -> AudioDevice {
        AudioDevice {
            id: 0,
            name: name.to_owned(),
            description: name.to_owned(),
            is_default,
            direction,
            form_factor: "speaker".into(),
        }
    }

    /// Builds a store with two real presets on disk, so `select_preset` has something to load.
    ///
    /// `tag` names the caller, because the directory has to be the caller's alone: these tests run
    /// on threads of one process, and a path shared between them meant each one wiped the presets
    /// another was in the middle of listing — a failure that only showed up under load.
    fn app_with_two_presets(tag: &str) -> App {
        use fxsound_core::Preset;

        let dir = std::env::temp_dir().join(format!(
            "fxsound-device-memory-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the preset directory");
        for name in ["Alpha", "Beta"] {
            let preset = Preset {
                name: name.to_owned(),
                ..Preset::default()
            };
            fxsound_preset::save(&preset, &dir.join(format!("{name}.fac"))).expect("write");
        }

        let mut app = App::headless_for_tests();
        app.presets = fxsound_preset::PresetStore::with_dirs(
            vec![dir],
            std::env::temp_dir().join(format!(
                "fxsound-device-memory-user-{}-{tag}",
                std::process::id()
            )),
        );
        app.presets.rescan();
        app.state.presets = app
            .presets
            .entries()
            .iter()
            .map(|e| fxsound_ui::state::PresetEntry {
                name: e.name.clone(),
                modified: e.modified,
                factory: true,
            })
            .collect();
        app.state.devices = vec![
            device("alsa_output.headphones", DeviceDirection::Output, false),
            device("alsa_output.speakers", DeviceDirection::Output, true),
        ];
        app
    }

    #[test]
    fn a_device_brings_back_the_preset_it_was_last_used_with() {
        let mut app = app_with_two_presets("restores");
        let index_of = |app: &App, name: &str| {
            app.state
                .presets
                .iter()
                .position(|e| e.name == name)
                .expect("the preset is listed")
        };

        let alpha = index_of(&app, "Alpha");
        let beta = index_of(&app, "Beta");

        app.handle(&[UiAction::SelectDevice(0), UiAction::SelectPreset(alpha)]);
        app.handle(&[UiAction::SelectDevice(1), UiAction::SelectPreset(beta)]);

        // Back to the first device: its own preset should come with it.
        app.handle(&[UiAction::SelectDevice(0)]);
        assert_eq!(
            app.state.preset().map(|p| p.name.as_str()),
            Some("Alpha"),
            "the headphones should have brought Alpha back"
        );

        app.handle(&[UiAction::SelectDevice(1)]);
        assert_eq!(
            app.state.preset().map(|p| p.name.as_str()),
            Some("Beta"),
            "and the speakers should have brought Beta back"
        );
    }

    #[test]
    fn a_device_seen_for_the_first_time_leaves_the_preset_alone() {
        // Guessing here would change the sound on no evidence. Only a remembered device restores.
        let mut app = app_with_two_presets("first-seen");
        let beta = app
            .state
            .presets
            .iter()
            .position(|e| e.name == "Beta")
            .expect("listed");

        app.handle(&[UiAction::SelectPreset(beta)]);
        app.handle(&[UiAction::SelectDevice(0)]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Beta"));
    }

    #[test]
    fn the_device_memory_records_what_kind_of_device_it_was() {
        let mut app = app_with_two_presets("form-factor");
        let alpha = app
            .state
            .presets
            .iter()
            .position(|e| e.name == "Alpha")
            .expect("listed");
        app.handle(&[UiAction::SelectDevice(0), UiAction::SelectPreset(alpha)]);

        let config = app
            .settings
            .device_configs
            .iter()
            .find(|c| c.device_id == "alsa_output.headphones")
            .expect("the device was remembered");
        assert_eq!(config.preset, "Alpha");
        assert_eq!(
            config.device_form_factor, "speaker",
            "the form factor was dead data before this"
        );
    }

    #[test]
    fn the_saved_device_is_announced_once_each_time_it_appears() {
        let mut settings = Settings::default();
        settings.set_device_name(DeviceDirection::Input, "alsa_input.usb-fifine");
        settings.set_lane_enabled(DeviceDirection::Input, true);
        let without = vec![device("alsa_output.pci", DeviceDirection::Output, true)];
        let with = vec![
            device("alsa_output.pci", DeviceDirection::Output, true),
            device("alsa_input.usb-fifine", DeviceDirection::Input, false),
        ];
        let mut announced = None;
        let input = DeviceDirection::Input;

        // Not listed yet: nothing to say.
        assert_eq!(
            saved_device_to_announce(&settings, &without, input, &mut announced),
            None
        );
        // Listed: announced exactly once…
        assert_eq!(
            saved_device_to_announce(&settings, &with, input, &mut announced),
            Some(UiToAudio::SelectDevice {
                node_name: "alsa_input.usb-fifine".to_owned(),
                direction: DeviceDirection::Input,
            })
        );
        assert_eq!(
            saved_device_to_announce(&settings, &with, input, &mut announced),
            None
        );
        // …and once more after it was unplugged and came back.
        assert_eq!(
            saved_device_to_announce(&settings, &without, input, &mut announced),
            None
        );
        assert!(saved_device_to_announce(&settings, &with, input, &mut announced).is_some());
    }

    #[test]
    fn a_saved_name_is_only_announced_in_its_own_direction() {
        let mut settings = Settings::default();
        settings.set_device_name(DeviceDirection::Input, "fifine");
        settings.set_lane_enabled(DeviceDirection::Input, true);
        let devices = vec![device("fifine", DeviceDirection::Output, true)];
        let mut announced = None;
        assert_eq!(
            saved_device_to_announce(&settings, &devices, DeviceDirection::Input, &mut announced),
            None
        );
        assert!(announced.is_none());
    }

    #[test]
    fn with_no_saved_device_nothing_is_announced() {
        let settings = Settings::default();
        let devices = vec![device("alsa_output.pci", DeviceDirection::Output, true)];
        for direction in DeviceDirection::ALL {
            let mut announced = None;
            assert_eq!(
                saved_device_to_announce(&settings, &devices, direction, &mut announced),
                None
            );
        }
    }

    /// The engine runs both lanes, so a start-up with both lanes on in the settings file brings
    /// both saved devices back — the speakers and the microphone, not one or the other.
    #[test]
    fn both_lanes_saved_devices_are_announced_when_both_lanes_were_on() {
        let mut settings = Settings::default();
        settings.set_device_name(DeviceDirection::Output, "alsa_output.pci");
        settings.set_device_name(DeviceDirection::Input, "alsa_input.usb-fifine");
        settings.set_lane_enabled(DeviceDirection::Input, true);
        // The window was last editing the speakers; that says nothing about the microphone's lane.
        settings.set_edit_direction(DeviceDirection::Output);
        let devices = vec![
            device("alsa_output.pci", DeviceDirection::Output, true),
            device("alsa_input.usb-fifine", DeviceDirection::Input, false),
        ];

        let mut announced = [None, None];
        let sent: Vec<UiToAudio> = DeviceDirection::ALL
            .into_iter()
            .filter_map(|direction| {
                saved_device_to_announce(
                    &settings,
                    &devices,
                    direction,
                    &mut announced[lane_index(direction)],
                )
            })
            .collect();
        assert_eq!(
            sent,
            [
                UiToAudio::SelectDevice {
                    node_name: "alsa_output.pci".to_owned(),
                    direction: DeviceDirection::Output,
                },
                UiToAudio::SelectDevice {
                    node_name: "alsa_input.usb-fifine".to_owned(),
                    direction: DeviceDirection::Input,
                },
            ]
        );
    }

    #[test]
    fn a_saved_microphone_is_not_announced_while_its_lane_is_off() {
        let mut settings = Settings::default();
        settings.set_device_name(DeviceDirection::Input, "alsa_input.usb-fifine");
        settings.set_lane_enabled(DeviceDirection::Input, false);
        let devices = vec![device(
            "alsa_input.usb-fifine",
            DeviceDirection::Input,
            true,
        )];
        let mut announced = None;
        assert_eq!(
            saved_device_to_announce(&settings, &devices, DeviceDirection::Input, &mut announced),
            None,
            "a detached lane stays detached across a restart"
        );
        assert!(announced.is_none());
    }

    // ---- what the engine is told about each lane ------------------------------------------

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;

    fn select(name: &str, direction: DeviceDirection) -> UiToAudio {
        UiToAudio::SelectDevice {
            node_name: name.to_owned(),
            direction,
        }
    }

    fn both_devices() -> Vec<AudioDevice> {
        vec![
            device("alsa_output.pci", OUT, true),
            device("alsa_input.usb-fifine", IN, false),
        ]
    }

    /// What a device list has the engine told about attaching lanes: its `SelectDevice`s, without
    /// the rankings (U4) the same list sends when it teaches the priority list something.
    fn announcements(app: &mut App, devices: Vec<AudioDevice>) -> Vec<UiToAudio> {
        app.adopt_device_list(devices)
            .into_iter()
            .filter(|message| matches!(message, UiToAudio::SelectDevice { .. }))
            .collect()
    }

    /// An app whose settings name a speaker and a microphone, the microphone lane switched `on` or
    /// off, and the window editing `edit` — the shape a restart comes back to.
    fn app_remembering(input_on: bool, edit: DeviceDirection) -> App {
        let mut app = headless();
        app.settings.set_device_name(OUT, "alsa_output.pci");
        app.settings.set_device_name(IN, "alsa_input.usb-fifine");
        app.settings.set_lane_enabled(IN, input_on);
        app.settings.set_edit_direction(edit);
        app.state.direction = edit;
        app
    }

    #[test]
    fn looking_at_the_microphone_list_then_a_device_list_sends_no_select_device() {
        let mut app = app_remembering(false, OUT);
        // Start-up: the speakers the settings name go to the engine, once.
        assert_eq!(
            announcements(&mut app, both_devices()),
            [select("alsa_output.pci", OUT)]
        );
        // Opening the other lane's list to look at it is a SetEditDirection and nothing more…
        app.handle(&[UiAction::SetEditDirection(IN)]);
        assert_eq!(app.settings.device_direction, IN);
        // …so the rescan that follows has nothing to tell the engine.
        assert_eq!(announcements(&mut app, both_devices()), []);
        assert_eq!(announcements(&mut app, both_devices()), []);
        assert_eq!(
            app.state.selected_input, None,
            "the microphone still shows Off"
        );
    }

    #[test]
    fn a_detached_microphone_is_not_attached_by_a_restart_that_was_editing_it() {
        // The settings file a restart reads after the user looked at the microphone list: the
        // edit direction says Input, the microphone lane says off.
        let mut app = app_remembering(false, IN);
        assert_eq!(
            announcements(&mut app, both_devices()),
            [select("alsa_output.pci", OUT)]
        );
        // With no speaker remembered either, the engine is left to its own device rules.
        let mut app = app_remembering(false, IN);
        app.settings.set_device_name(OUT, "");
        assert_eq!(announcements(&mut app, both_devices()), []);
    }

    #[test]
    fn a_restart_with_both_lanes_on_announces_both_saved_devices_whatever_the_window_edited() {
        for edit in [OUT, IN] {
            let mut app = app_remembering(true, edit);
            assert_eq!(
                announcements(&mut app, both_devices()),
                [
                    select("alsa_output.pci", OUT),
                    select("alsa_input.usb-fifine", IN)
                ],
                "editing {edit:?}"
            );
        }
    }

    #[test]
    fn with_the_speakers_switched_off_only_the_microphone_is_announced() {
        let mut app = app_remembering(true, OUT);
        app.settings.set_lane_enabled(OUT, false);
        assert_eq!(
            announcements(&mut app, both_devices()),
            [select("alsa_input.usb-fifine", IN)]
        );
    }

    #[test]
    fn with_both_lanes_off_nothing_is_announced() {
        let mut app = app_remembering(false, IN);
        app.settings.set_lane_enabled(OUT, false);
        assert_eq!(announcements(&mut app, both_devices()), []);
    }

    #[test]
    fn a_start_with_the_speakers_switched_off_detaches_their_lane_before_anything_else() {
        let mut settings = Settings::default();
        assert!(
            !startup_messages(&settings).contains(&UiToAudio::DetachLane(OUT)),
            "the engine starts with the speakers' lane on, and a fresh install keeps it"
        );
        settings.set_lane_enabled(OUT, false);
        let messages = startup_messages(&settings);
        assert!(messages.contains(&UiToAudio::DetachLane(OUT)));
        assert!(
            !messages.contains(&UiToAudio::DetachLane(IN)),
            "the input lane starts detached and needs no word"
        );
        assert!(
            matches!(messages[0], UiToAudio::SeedRememberedDefaults { .. }),
            "what a killed run displaced still goes first"
        );
    }

    #[test]
    fn each_lane_is_announced_once_wherever_the_window_looks() {
        let mut app = app_remembering(true, OUT);
        app.state.devices = both_devices();
        app.handle(&[UiAction::SelectDevice(1)]);
        app.handle(&[UiAction::SetEditDirection(OUT)]);
        assert_eq!(app.settings.device_direction, OUT);
        // Headless, so the pick itself sent nothing; the list after it names each enabled lane's
        // device once, the lane the window moved away from included.
        assert_eq!(
            announcements(&mut app, both_devices()),
            [
                select("alsa_output.pci", OUT),
                select("alsa_input.usb-fifine", IN)
            ]
        );
        app.handle(&[UiAction::SetEditDirection(IN)]);
        assert_eq!(announcements(&mut app, both_devices()), []);
    }

    #[test]
    fn a_device_list_that_changes_around_the_saved_devices_sends_nothing_again() {
        let mut app = app_remembering(true, IN);
        assert_eq!(announcements(&mut app, both_devices()).len(), 2);
        let mut more = both_devices();
        more.push(device("bluez_output.headset", OUT, false));
        assert_eq!(announcements(&mut app, more), []);
    }

    #[test]
    fn switching_off_the_microphone_hands_nothing_to_the_speakers() {
        let mut app = app_remembering(true, OUT);
        app.state.devices = both_devices();
        app.handle(&[UiAction::SelectDevice(1)]);
        // What a send would have recorded, since a headless app has no engine to send to.
        app.announced_device = [
            Some("alsa_output.pci".to_owned()),
            Some("alsa_input.usb-fifine".to_owned()),
        ];

        app.handle(&[UiAction::DetachInput]);

        assert!(!app.settings.lane_enabled(IN));
        assert_eq!(
            app.announced_device,
            [Some("alsa_output.pci".to_owned()), None],
            "the speakers' record is untouched"
        );
        // The engine runs the speakers' lane already; there is nothing to move it to.
        assert_eq!(app.saved_devices_for_engine(), []);
    }

    #[test]
    fn a_lane_switched_back_on_is_announced_again() {
        let mut app = app_remembering(true, OUT);
        app.state.devices = both_devices();
        app.announced_device = [
            Some("alsa_output.pci".to_owned()),
            Some("alsa_input.usb-fifine".to_owned()),
        ];
        app.handle(&[UiAction::DetachInput]);
        app.settings.set_lane_enabled(IN, true);
        assert_eq!(
            app.saved_devices_for_engine(),
            [select("alsa_input.usb-fifine", IN)]
        );
    }

    #[test]
    fn a_detached_lane_is_forgotten_by_the_announcement_bookkeeping() {
        let mut settings = Settings::default();
        settings.set_device_name(IN, "alsa_input.usb-fifine");
        settings.set_lane_enabled(IN, false);
        let mut announced = Some("alsa_input.usb-fifine".to_owned());
        assert_eq!(
            saved_device_to_announce(&settings, &both_devices(), IN, &mut announced),
            None
        );
        assert_eq!(announced, None);
    }

    #[test]
    fn picking_a_device_records_it_as_announced() {
        let mut app = headless();
        app.state.devices = vec![
            device("alsa_output.pci", DeviceDirection::Output, true),
            device("alsa_input.usb-fifine", DeviceDirection::Input, false),
        ];
        app.handle(&[UiAction::SelectDevice(1)]);
        assert_eq!(
            app.settings.device_name(DeviceDirection::Input),
            "alsa_input.usb-fifine"
        );
        assert_eq!(app.settings.device_direction, DeviceDirection::Input);
        // No engine in a headless app, so nothing was sent and nothing is on record; the
        // announcement bookkeeping only follows an actual send.
        assert_eq!(app.announced_device, [None, None]);
    }

    #[test]
    fn picking_a_microphone_switches_its_lane_on_and_leaves_the_speakers_lane_alone() {
        let mut app = headless();
        app.settings.set_lane_enabled(DeviceDirection::Input, false);
        app.state.devices = vec![
            device("alsa_output.pci", DeviceDirection::Output, true),
            device("alsa_input.usb-fifine", DeviceDirection::Input, false),
        ];

        app.handle(&[UiAction::SelectDevice(1)]);
        assert!(
            app.settings.lane_enabled(DeviceDirection::Input),
            "the next start brings the microphone back"
        );
        assert!(app.settings.lane_enabled(DeviceDirection::Output));

        // And picking speakers afterwards says nothing about the microphone.
        app.handle(&[UiAction::SelectDevice(0)]);
        assert!(app.settings.lane_enabled(DeviceDirection::Input));
        assert_eq!(
            app.settings.device_name(DeviceDirection::Input),
            "alsa_input.usb-fifine"
        );
    }

    #[test]
    fn a_status_says_whether_its_own_lane_is_active_and_nothing_about_the_other() {
        let mut app = headless();
        app.settings.set_lane_enabled(DeviceDirection::Input, true);
        app.state.output_active = true;
        let status = fxsound_core::AudioStatus {
            processing: true,
            sample_rate: 16_000,
            channels: 1,
            ..fxsound_core::AudioStatus::default()
        };
        app.receive(AudioToUi::Status {
            direction: DeviceDirection::Input,
            status,
        });
        assert!(app.state.input_active);
        assert!(app.state.output_active, "the speakers said nothing");
        assert_eq!(app.audio_status_for(DeviceDirection::Input), &status);
        assert_eq!(
            app.audio_status_for(DeviceDirection::Output),
            &fxsound_core::AudioStatus::default()
        );

        app.receive(AudioToUi::Status {
            direction: DeviceDirection::Output,
            status: fxsound_core::AudioStatus {
                processing: false,
                ..status
            },
        });
        assert!(!app.state.output_active);
        assert!(app.state.input_active);
    }

    #[test]
    fn a_lane_that_loses_its_nodes_stops_being_active_at_once() {
        let mut app = lanes();
        app.state.output_active = true;
        app.state.input_active = true;
        app.receive(AudioToUi::Attached {
            direction: DeviceDirection::Input,
            node_name: None,
        });
        assert!(!app.state.input_active);
        assert!(app.state.output_active, "the other lane is its own");
        assert_eq!(app.attached(DeviceDirection::Input), None);
    }

    #[test]
    fn the_picture_is_the_edited_lanes_and_the_microphone_telemetry_the_input_lanes() {
        let music = Meters {
            active: true,
            sample_rate: 48_000,
            gate_reduction_db: 0.0,
            noise_floor_db: 0.0,
            ..Meters::default()
        };
        let mut voice = Meters {
            active: false,
            sample_rate: 16_000,
            gate_reduction_db: 7.5,
            noise_floor_db: -52.0,
            voice_probability: 0.9,
            denoise_reduction_db: 11.0,
            ..Meters::default()
        };
        voice.spectrum[0] = 0.5;

        // Editing the speakers: the analyser and the band limit are the music chain's, and the
        // strip still reads the microphone.
        let mut state = UiState::default();
        show_meters(&mut state, &music, &voice);
        assert!(state.audio_active);
        assert_eq!(state.sample_rate, 48_000);
        assert_eq!(state.spectrum, music.spectrum);
        assert_eq!(state.gate_reduction_db, 7.5);
        assert_eq!(state.noise_floor_db, -52.0);
        assert_eq!(state.voice_probability, 0.9);
        assert_eq!(state.denoise_reduction_db, 11.0);

        // Editing the microphone: both are the voice chain's.
        show_meters(&mut state, &voice, &voice);
        assert!(!state.audio_active);
        assert_eq!(state.sample_rate, 16_000);
        assert_eq!(state.spectrum, voice.spectrum);
        assert_eq!(state.gate_reduction_db, 7.5);
    }

    #[test]
    fn an_engine_error_and_a_disconnect_are_shown_as_notices() {
        let mut app = headless();
        app.receive(AudioToUi::Disconnected {
            reason: "gone".to_owned(),
        });
        assert!(
            app.state
                .notification
                .as_deref()
                .is_some_and(|text| text.ends_with("gone")),
            "{:?}",
            app.state.notification
        );
        app.receive(AudioToUi::Error {
            direction: Some(DeviceDirection::Input),
            message: "no microphone".to_owned(),
        });
        assert_eq!(app.state.notification.as_deref(), Some("no microphone"));
    }

    #[test]
    fn the_status_shown_is_the_edited_lanes_and_each_lane_keeps_its_own() {
        let mut app = headless();
        app.audio_status[lane_index(DeviceDirection::Output)].channels = 8;
        app.audio_status[lane_index(DeviceDirection::Input)].sample_rate = 16_000;
        app.attached[lane_index(DeviceDirection::Input)] = Some("alsa_input.usb-fifine".to_owned());

        app.state.direction = DeviceDirection::Output;
        assert_eq!(app.audio_status().channels, 8);
        app.state.direction = DeviceDirection::Input;
        assert_eq!(app.audio_status().sample_rate, 16_000);
        assert_eq!(app.audio_status().channels, 2, "not the output lane's");

        assert_eq!(app.audio_status_for(DeviceDirection::Output).channels, 8);
        assert_eq!(
            app.attached(DeviceDirection::Input),
            Some("alsa_input.usb-fifine")
        );
        assert_eq!(app.attached(DeviceDirection::Output), None);
    }

    // ---- presets on disk: rename, import, export -------------------------------------------

    /// An app whose preset store and export directory live in a scratch directory of their own,
    /// so these tests can run in parallel and never touch the user's files.
    fn with_store() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("scratch directory");
        let mut app = headless();
        app.presets = PresetStore::with_dirs(Vec::new(), dir.path().join("user"));
        app.export_dir = dir.path().join("export");
        (app, dir)
    }

    fn add_user_preset(app: &mut App, name: &str) {
        let preset = Preset {
            name: name.to_owned(),
            ..Preset::default()
        };
        app.presets.save_as(&preset, name).expect("preset saved");
        app.refresh_preset_list();
    }

    fn names(app: &App) -> Vec<&str> {
        app.state.presets.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn renaming_a_user_preset_moves_the_file_and_keeps_it_selected() {
        let (mut app, dir) = with_store();
        add_user_preset(&mut app, "Mine");
        app.select_preset(0);
        assert!(app.loaded_preset.is_some());

        app.rename_preset("Yours");

        assert_eq!(names(&app), ["Yours"]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Yours"));
        assert!(!app.state.preset().is_some_and(|p| p.modified));
        assert_eq!(app.settings.selected_preset(), "Yours");
        assert_eq!(
            app.loaded_preset.as_ref().map(|p| p.name.as_str()),
            Some("Yours")
        );
        assert!(dir.path().join("user/Yours.fac").is_file());
        assert!(!dir.path().join("user/Mine.fac").exists());
    }

    #[test]
    fn a_factory_preset_refuses_to_be_renamed() {
        let mut app = with_presets(&["Jazz"]);
        app.rename_preset("Blues");
        assert_eq!(names(&app), ["Jazz"]);
        assert!(app.state.notification.is_some());
    }

    #[test]
    fn renaming_onto_an_existing_name_is_refused_case_insensitively() {
        let (mut app, _dir) = with_store();
        add_user_preset(&mut app, "Alpha");
        add_user_preset(&mut app, "Beta");
        app.select_preset(0);

        app.rename_preset("beta");

        assert_eq!(names(&app), ["Alpha", "Beta"]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Alpha"));
        assert!(app.state.notification.is_some());
        assert!(!app.is_preset_name_available("BETA"));
        assert!(app.is_preset_name_available("Gamma"));
        assert!(!app.is_preset_name_available("   "));
    }

    #[test]
    fn renaming_to_the_same_or_an_empty_name_changes_nothing() {
        let (mut app, dir) = with_store();
        add_user_preset(&mut app, "Mine");
        app.select_preset(0);

        app.rename_preset("Mine");
        app.rename_preset("   ");

        assert_eq!(names(&app), ["Mine"]);
        assert!(dir.path().join("user/Mine.fac").is_file());
        assert!(app.state.notification.is_none());
    }

    #[test]
    fn importing_a_folder_copies_new_presets_and_skips_taken_names() {
        let (mut app, dir) = with_store();
        add_user_preset(&mut app, "Mine");
        app.select_preset(0);

        let incoming = dir.path().join("incoming");
        std::fs::create_dir_all(&incoming).unwrap();
        for name in ["mine", "Fresh"] {
            let preset = Preset {
                name: name.to_owned(),
                ..Preset::default()
            };
            fxsound_preset::save(&preset, &incoming.join(format!("{name}.fac"))).unwrap();
        }
        std::fs::write(incoming.join("notes.txt"), "not a preset").unwrap();

        let mut state = ImportState {
            folder: Some(incoming),
            ..ImportState::default()
        };
        assert!(!app.handle_import(&PresetsAction::Import, &mut state));

        let summary = state.summary.clone().expect("import ran");
        assert_eq!(summary.imported, ["Fresh"]);
        assert_eq!(summary.skipped, ["mine"]);
        assert_eq!(names(&app), ["Fresh", "Mine"]);
        // The selection followed the preset, not the index.
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Mine"));
        assert!(dir.path().join("user/Fresh.fac").is_file());
        assert!(app.handle_import(&PresetsAction::CloseImport, &mut state));
    }

    #[test]
    fn importing_a_folder_without_presets_leaves_a_notice_and_the_window_open() {
        let (mut app, dir) = with_store();
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();

        let mut state = ImportState {
            folder: Some(empty),
            ..ImportState::default()
        };
        assert!(!app.handle_import(&PresetsAction::Import, &mut state));
        assert!(state.summary.is_none());
        assert_eq!(
            state.notice.as_deref(),
            Some(fxsound_ui::dialogs::presets::NO_PRESETS_FOUND)
        );
        assert!(!app.handle_import(&PresetsAction::DismissNotice, &mut state));
        assert!(state.notice.is_none());
    }

    #[test]
    fn exporting_writes_files_then_asks_once_about_collisions() {
        let (mut app, dir) = with_store();
        add_user_preset(&mut app, "Mine");
        add_user_preset(&mut app, "Other");

        let mut state = ExportState {
            presets: vec!["Mine".into(), "Other".into()],
            ..ExportState::default()
        };
        app.handle_export(&PresetsAction::ToggleExport(0), &mut state);
        app.handle_export(&PresetsAction::ToggleExport(1), &mut state);
        app.handle_export(&PresetsAction::ToggleExport(1), &mut state);
        assert_eq!(state.selected_names(), ["Mine"]);

        assert!(!app.handle_export(&PresetsAction::Export, &mut state));
        assert!(state.exporting);
        assert!(state.collisions.is_empty());
        assert_eq!(state.finished, Some(true));
        assert!(dir.path().join("export/Mine.fac").is_file());
        assert!(app.handle_export(&PresetsAction::CloseExport, &mut state));

        // Second time round the file is there, so the prompt comes up instead of a write.
        let mut state = ExportState {
            presets: vec!["Mine".into(), "Other".into()],
            selected: [0, 1].into_iter().collect(),
            ..ExportState::default()
        };
        app.handle_export(&PresetsAction::Export, &mut state);
        assert_eq!(state.collisions, ["Mine"]);
        assert_eq!(state.finished, None);

        // "No" exports only what does not collide.
        app.handle_export(
            &PresetsAction::Overwrite(OverwriteChoice::SkipAll),
            &mut state,
        );
        assert!(state.collisions.is_empty());
        assert_eq!(state.finished, Some(true));
        assert!(dir.path().join("export/Other.fac").is_file());

        // Cancel writes nothing.
        let mut state = ExportState {
            presets: vec!["Mine".into()],
            selected: [0].into_iter().collect(),
            ..ExportState::default()
        };
        app.handle_export(&PresetsAction::Export, &mut state);
        app.handle_export(
            &PresetsAction::Overwrite(OverwriteChoice::Cancel),
            &mut state,
        );
        assert_eq!(state.finished, Some(false));
    }

    #[test]
    fn the_user_preset_cap_is_clamped_the_way_the_original_clamps_it() {
        let mut app = headless();
        assert_eq!(app.max_user_presets(), 120);
        app.settings.max_user_presets = 50;
        assert_eq!(app.max_user_presets(), 50);
        app.settings.max_user_presets = 3;
        assert_eq!(app.max_user_presets(), 120);
        app.settings.max_user_presets = 500;
        assert_eq!(app.max_user_presets(), 120);
    }

    #[test]
    fn the_export_asks_about_exactly_the_file_the_store_writes() {
        // The collision check used to keep its own copy of the naming rule, which the store had
        // since replaced: `a/b` was looked for as `a_b.fac` while `ab.fac` was the file written,
        // so an export replaced it without asking. It now asks the store.
        let (mut app, dir) = with_store();
        add_user_preset(&mut app, "Mu:sic?");
        std::fs::create_dir_all(dir.path().join("export")).unwrap();
        std::fs::write(dir.path().join("export/Music.fac"), "an earlier export").unwrap();

        let mut state = ExportState {
            presets: vec!["Mu:sic?".into()],
            selected: [0].into_iter().collect(),
            ..ExportState::default()
        };
        app.handle_export(&PresetsAction::Export, &mut state);
        assert_eq!(state.collisions, ["Mu:sic?"], "asked before replacing it");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("export/Music.fac")).unwrap(),
            "an earlier export"
        );
    }

    // ---- the Settings pane's microphone settings (0.4.0 design §1.4, §2 & 3, §7) -------------

    use fxsound_core::{
        DeEsserMode, DenoiseChannelMode, DenoiseChannelsOverride, DenoiseLevel, DereverbLevel,
        NoiseSuppressionOverride,
    };
    use fxsound_ui::dialogs::settings::SettingsAction;

    /// A preset's voicing: a Linked, Light denoiser with a tuned row, the adaptive de-esser and a
    /// Light de-reverb.
    fn tuned_voicing() -> PresetVoicing {
        PresetVoicing {
            rnnoise: true,
            denoise_level: DenoiseLevel::Light,
            denoise_channels: DenoiseChannelMode::Linked,
            denoise_control: fxsound_core::DenoiseControl {
                max_suppression_db: 9.0,
                ..DenoiseLevel::Light.control()
            },
            deesser_mode: DeEsserMode::Adaptive,
            dereverb: DereverbLevel::Light,
        }
    }

    /// Default settings with one change; `Settings` has a private field, so no struct update.
    fn settings_with(change: impl FnOnce(&mut Settings)) -> Settings {
        let mut settings = Settings::default();
        change(&mut settings);
        settings
    }

    fn applied(voicing: &PresetVoicing, settings: &Settings) -> InputDspParams {
        let mut params = unvoiced_input_params();
        apply_microphone_settings(&mut params, voicing, settings);
        params
    }

    #[test]
    fn with_every_setting_at_its_default_the_voice_chain_runs_what_the_preset_says() {
        let voicing = tuned_voicing();
        let params = applied(&voicing, &Settings::default());
        assert_eq!(PresetVoicing::of(&params), voicing);
        // And an unvoiced chain stays unvoiced: the defaults switch nothing on.
        let unvoiced = PresetVoicing::of(&unvoiced_input_params());
        let params = applied(&unvoiced, &Settings::default());
        assert!(!params.rnnoise);
        assert_eq!(params.dereverb, DereverbLevel::Off);
        assert_eq!(params.deesser_mode, DeEsserMode::Classic);
    }

    #[test]
    fn a_pinned_noise_suppression_level_wins_and_brings_its_own_row() {
        let voicing = tuned_voicing();
        let settings = settings_with(|s| s.noise_suppression = NoiseSuppressionOverride::Strong);
        let params = applied(&voicing, &settings);
        assert_eq!(params.denoise_level, DenoiseLevel::Strong);
        assert_eq!(params.denoise_control, DenoiseLevel::Strong.control());
        assert!(params.rnnoise);
        // The channel mode is a separate override and still follows the preset.
        assert_eq!(params.denoise_channels, DenoiseChannelMode::Linked);
    }

    #[test]
    fn pinning_the_level_the_preset_already_names_keeps_the_presets_tuned_row() {
        let voicing = tuned_voicing();
        let settings = settings_with(|s| s.noise_suppression = NoiseSuppressionOverride::Light);
        let params = applied(&voicing, &settings);
        assert_eq!(params.denoise_level, DenoiseLevel::Light);
        assert_eq!(params.denoise_control.max_suppression_db, 9.0);
    }

    #[test]
    fn a_pinned_level_runs_the_denoiser_on_a_preset_without_one_and_off_stops_it_on_one_with() {
        let bare = PresetVoicing {
            rnnoise: false,
            denoise_level: DenoiseLevel::Off,
            ..tuned_voicing()
        };
        let medium = settings_with(|s| s.noise_suppression = NoiseSuppressionOverride::Medium);
        let params = applied(&bare, &medium);
        assert!(params.rnnoise, "the override switched the stage on");
        assert_eq!(params.denoise_level, DenoiseLevel::Medium);
        assert_eq!(params.denoise_control, DenoiseLevel::Medium.control());

        let off = settings_with(|s| s.noise_suppression = NoiseSuppressionOverride::Off);
        let params = applied(&tuned_voicing(), &off);
        assert!(!params.rnnoise, "Off is off, whatever the preset says");
        assert_eq!(params.denoise_level, DenoiseLevel::Off);
        assert!(!params.denoise_control.is_active());
    }

    #[test]
    fn the_channel_override_wins_over_the_presets_mode() {
        for (choice, mode) in [
            (DenoiseChannelsOverride::Mono, DenoiseChannelMode::Mono),
            (DenoiseChannelsOverride::Linked, DenoiseChannelMode::Linked),
            (
                DenoiseChannelsOverride::Independent,
                DenoiseChannelMode::Independent,
            ),
            (DenoiseChannelsOverride::Preset, DenoiseChannelMode::Linked),
        ] {
            let settings = settings_with(|s| s.denoise_channels = choice);
            assert_eq!(
                applied(&tuned_voicing(), &settings).denoise_channels,
                mode,
                "{choice:?}"
            );
        }
    }

    #[test]
    fn the_adaptive_de_esser_is_asked_for_by_either_the_setting_or_the_preset() {
        let classic_preset = PresetVoicing {
            deesser_mode: DeEsserMode::Classic,
            ..tuned_voicing()
        };
        let adaptive_setting = settings_with(|s| s.deesser_mode = DeEsserMode::Adaptive);
        assert_eq!(
            applied(&classic_preset, &adaptive_setting).deesser_mode,
            DeEsserMode::Adaptive
        );
        assert_eq!(
            applied(&classic_preset, &Settings::default()).deesser_mode,
            DeEsserMode::Classic
        );
        // A preset that asks for it (Gaming Headset does) keeps it under the default setting.
        assert_eq!(
            applied(&tuned_voicing(), &Settings::default()).deesser_mode,
            DeEsserMode::Adaptive
        );
    }

    #[test]
    fn the_de_reverb_runs_at_the_stronger_of_the_setting_and_the_preset() {
        let light_preset = tuned_voicing();
        for (setting, expected) in [
            (DereverbLevel::Off, DereverbLevel::Light),
            (DereverbLevel::Light, DereverbLevel::Light),
            (DereverbLevel::Medium, DereverbLevel::Medium),
            (DereverbLevel::Strong, DereverbLevel::Strong),
        ] {
            let settings = settings_with(|s| s.dereverb = setting);
            assert_eq!(
                applied(&light_preset, &settings).dereverb,
                expected,
                "{setting:?}"
            );
        }
    }

    #[test]
    fn a_microphone_setting_reaches_the_voice_chain_while_the_speakers_are_being_edited() {
        let mut app = headless();
        assert_eq!(app.state.direction, DeviceDirection::Output);
        let mut pane = app.settings_state();
        for action in [
            SettingsAction::SetNoiseSuppression(NoiseSuppressionOverride::Strong),
            SettingsAction::SetDenoiseChannels(DenoiseChannelsOverride::Mono),
            SettingsAction::SetDeEsserMode(DeEsserMode::Adaptive),
            SettingsAction::SetDereverb(DereverbLevel::Medium),
        ] {
            app.handle_settings(&action, &mut pane);
        }

        // Written to the settings, to the pane's working copy, and published.
        assert_eq!(
            app.settings().noise_suppression,
            NoiseSuppressionOverride::Strong
        );
        assert_eq!(
            app.settings().denoise_channels,
            DenoiseChannelsOverride::Mono
        );
        assert_eq!(app.settings().deesser_mode, DeEsserMode::Adaptive);
        assert_eq!(app.settings().dereverb, DereverbLevel::Medium);
        assert_eq!(
            pane.settings.noise_suppression,
            NoiseSuppressionOverride::Strong
        );
        assert_eq!(
            pane.settings.denoise_channels,
            DenoiseChannelsOverride::Mono
        );
        assert_eq!(pane.settings.deesser_mode, DeEsserMode::Adaptive);
        assert_eq!(pane.settings.dereverb, DereverbLevel::Medium);

        let input = app.input_params();
        assert!(input.rnnoise);
        assert_eq!(input.denoise_level, DenoiseLevel::Strong);
        assert_eq!(input.denoise_channels, DenoiseChannelMode::Mono);
        assert_eq!(input.deesser_mode, DeEsserMode::Adaptive);
        assert_eq!(input.dereverb, DereverbLevel::Medium);
        // The readout strip hears about it too.
        assert!(app.state.denoise_on);
        assert_eq!(app.state.denoise_level, DenoiseLevel::Strong);
        assert!(app.state.dereverb_on);
    }

    #[test]
    fn setting_an_override_back_to_preset_restores_what_the_preset_said() {
        use fxsound_preset::input::{Denoise, InputPreset};
        let mut app = headless();
        app.state.direction = DeviceDirection::Input;
        let _voices = use_voices(
            &mut app,
            &[InputPreset {
                name: "Tuned".to_owned(),
                rnnoise: true,
                denoise: Some(Denoise {
                    level: DenoiseLevel::Light,
                    channels: DenoiseChannelMode::Linked,
                    max_suppression_db: Some(9.0),
                    ..Denoise::default()
                }),
                ..InputPreset::default()
            }],
        );
        app.refresh_preset_list();
        app.handle(&[UiAction::SelectPreset(0)]);
        let voiced = *app.input_params();
        assert_eq!(voiced.denoise_control.max_suppression_db, 9.0);

        let mut pane = app.settings_state();
        app.handle_settings(
            &SettingsAction::SetNoiseSuppression(NoiseSuppressionOverride::Off),
            &mut pane,
        );
        app.handle_settings(
            &SettingsAction::SetDenoiseChannels(DenoiseChannelsOverride::Independent),
            &mut pane,
        );
        assert!(!app.input_params().rnnoise);
        assert!(!app.state.denoise_on, "the strip says the denoiser is off");
        assert_eq!(
            app.input_params().denoise_channels,
            DenoiseChannelMode::Independent
        );

        app.handle_settings(
            &SettingsAction::SetNoiseSuppression(NoiseSuppressionOverride::Preset),
            &mut pane,
        );
        app.handle_settings(
            &SettingsAction::SetDenoiseChannels(DenoiseChannelsOverride::Preset),
            &mut pane,
        );
        assert_eq!(
            *app.input_params(),
            voiced,
            "the preset's own voicing is back"
        );
        assert!(app.state.denoise_on);
    }

    #[test]
    fn an_override_survives_picking_another_voice_preset() {
        let mut app = headless();
        let _voices = with_voice_presets(&mut app);
        app.state.direction = DeviceDirection::Input;
        app.refresh_preset_list();
        app.settings.noise_suppression = NoiseSuppressionOverride::Medium;
        for index in [0, 1, 0] {
            app.handle(&[UiAction::SelectPreset(index)]);
            // Neither test preset has a denoiser of its own.
            assert!(app.input_params().rnnoise, "preset {index}");
            assert_eq!(app.input_params().denoise_level, DenoiseLevel::Medium);
        }
    }

    #[test]
    fn echo_cancellation_is_saved_shown_and_left_to_the_audio_thread_to_confirm() {
        let mut app = headless();
        let mut pane = app.settings_state();
        app.handle_settings(&SettingsAction::SetEchoCancel(true), &mut pane);
        assert!(app.settings().echo_cancel);
        assert!(pane.settings.echo_cancel);
        assert!(app.state.echo_cancel_on, "the strip knows it was asked for");
        // Nothing has confirmed it: with no audio thread there is nobody to load the module.
        assert!(!app.state.echo_cancel_running);
        assert_eq!(pane.echo_cancel_status().as_deref(), Some("unavailable"));

        app.handle_settings(&SettingsAction::SetEchoCancel(false), &mut pane);
        assert!(!app.settings().echo_cancel);
        assert!(!app.state.echo_cancel_on);
        assert_eq!(pane.echo_cancel_status(), None);
    }

    #[test]
    fn a_saved_echo_cancellation_is_asked_of_the_engine_as_soon_as_it_exists() {
        // The restart the verifier described: `echo_cancel = true` in settings.toml, and an engine
        // that starts knowing nothing of it.
        let dir = std::env::temp_dir().join(format!("fxsound-startup-echo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("settings.toml");
        let mut saved = Settings::default();
        saved.echo_cancel = true;
        saved.save_to(&path).expect("save");
        let loaded = Settings::load_from(&path);
        let _ = std::fs::remove_dir_all(&dir);

        let messages = startup_messages(&loaded);
        let asked = messages
            .iter()
            .filter(|m| **m == UiToAudio::SetEchoCancel(true))
            .count();
        assert_eq!(
            asked, 1,
            "sent once, not zero times and not twice: {messages:?}"
        );
    }

    #[test]
    fn echo_cancellation_left_off_is_not_mentioned_to_a_new_engine() {
        let messages = startup_messages(&Settings::default());
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, UiToAudio::SetEchoCancel(_))),
            "off is where the engine starts: {messages:?}"
        );
    }

    #[test]
    fn the_remembered_defaults_are_still_the_first_thing_a_new_engine_hears() {
        let mut settings = Settings::default();
        settings.echo_cancel = true;
        settings.remembered_default_output = "alsa_output.speakers".to_owned();
        settings.remembered_default_input = "alsa_input.headset".to_owned();
        assert_eq!(
            startup_messages(&settings),
            [
                UiToAudio::SeedRememberedDefaults {
                    output: "alsa_output.speakers".to_owned(),
                    input: "alsa_input.headset".to_owned(),
                },
                UiToAudio::SeedTargetVolumes(Vec::new()),
                UiToAudio::SetDevicePriority {
                    direction: DeviceDirection::Output,
                    names: Vec::new(),
                },
                UiToAudio::SetDevicePriority {
                    direction: DeviceDirection::Input,
                    names: Vec::new(),
                },
                UiToAudio::SetEchoCancel(true),
            ]
        );
    }

    #[test]
    fn echo_cancellation_ticked_in_the_pane_is_asked_for_again_by_the_next_run() {
        let mut app = headless();
        let mut pane = app.settings_state();
        app.handle_settings(&SettingsAction::SetEchoCancel(true), &mut pane);
        assert!(startup_messages(app.settings()).contains(&UiToAudio::SetEchoCancel(true)));

        app.handle_settings(&SettingsAction::SetEchoCancel(false), &mut pane);
        assert!(!startup_messages(app.settings()).contains(&UiToAudio::SetEchoCancel(true)));
    }

    #[test]
    fn the_pane_is_refreshed_with_what_the_audio_thread_said_about_echo_cancellation() {
        let mut app = headless();
        let mut pane = app.settings_state();
        pane.settings.echo_cancel = true;
        app.state.echo_cancel_running = false;
        app.echo_cancel_detail = "no libspa-aec-webrtc".to_owned();
        app.refresh_settings_state(&mut pane);
        assert_eq!(
            pane.echo_cancel_status().as_deref(),
            Some("unavailable · no libspa-aec-webrtc")
        );
        app.state.echo_cancel_running = true;
        app.refresh_settings_state(&mut pane);
        assert_eq!(pane.echo_cancel_status(), None);
    }

    #[test]
    fn calibration_is_offered_only_with_a_microphone_attached() {
        let mut app = headless();
        app.state.devices = vec![
            device("alsa_output.speakers", DeviceDirection::Output, true),
            device("alsa_input.mic", DeviceDirection::Input, false),
        ];
        app.state.selected_output = Some(0);
        assert!(!app.settings_state().has_microphone);
        assert_eq!(app.microphone_description(), None);

        // Picked is not attached: the wizard measures the lane's meters, and there are none yet.
        app.handle(&[UiAction::SelectInput(1)]);
        assert!(!app.settings_state().has_microphone);

        app.receive(AudioToUi::Attached {
            direction: DeviceDirection::Input,
            node_name: Some("alsa_input.mic".to_owned()),
        });
        assert!(app.settings_state().has_microphone);
        assert_eq!(app.microphone_description(), Some("alsa_input.mic"));

        // A name that the list carries only as a speaker is not a microphone.
        app.receive(AudioToUi::Attached {
            direction: DeviceDirection::Input,
            node_name: Some("alsa_output.speakers".to_owned()),
        });
        assert!(!app.settings_state().has_microphone);

        app.receive(AudioToUi::Attached {
            direction: DeviceDirection::Input,
            node_name: None,
        });
        assert!(!app.settings_state().has_microphone);
    }

    #[test]
    fn opening_the_wizard_changes_no_setting() {
        let mut app = headless();
        let mut pane = app.settings_state();
        let before = (app.settings().clone(), pane.clone());
        app.handle_settings(&SettingsAction::OpenCalibration, &mut pane);
        assert_eq!((app.settings().clone(), pane), before);
    }

    // ---- the controller on the two-lane engine (0.4.0 design §1.4), through a fake feed --------

    use crate::audio_link::FakeEngine;
    use fxsound_core::messages::TargetVolume;

    const SPEAKERS: &str = "alsa_output.speakers";
    const HEADPHONES: &str = "alsa_output.headphones";
    const MIC: &str = "alsa_input.mic";

    /// Speakers, headphones and a microphone, as the engine lists them.
    fn two_lane_devices() -> Vec<AudioDevice> {
        vec![
            device(SPEAKERS, OUT, true),
            device(HEADPHONES, OUT, false),
            device(MIC, IN, true),
        ]
    }

    /// A music store on disk holding a flat `Alpha` and a bass-heavy `Beta`, kept alive by the
    /// returned directory.
    fn music_store() -> (PresetStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("scratch directory");
        let factory = dir.path().join("factory");
        std::fs::create_dir_all(&factory).expect("factory directory");
        for (name, bass) in [("Alpha", 0.0), ("Beta", 0.6)] {
            let mut preset = Preset {
                name: name.to_owned(),
                ..Preset::default()
            };
            preset.set_effect(Effect::Bass, bass);
            fxsound_preset::save(&preset, &factory.join(format!("{name}.fac"))).expect("write");
        }
        let mut store = PresetStore::with_dirs(vec![factory], dir.path().join("user"));
        store.rescan();
        (store, dir)
    }

    /// A voice store in `dir` holding [`voice_list`] as its factory set, with the user's voice
    /// presets in `user/Input`, the layout the real stores use beside [`music_store`]'s.
    fn voices(dir: &tempfile::TempDir) -> InputPresetStore {
        voice_store_for_tests(
            &voice_list(),
            &dir.path().join("voice-factory"),
            dir.path().join("user").join("Input"),
        )
    }

    /// Two voice presets: `Loud`, with 9 dB of makeup and the podcast chain, and `Quiet`, with
    /// none and a 75 Hz high-pass.
    fn voice_list() -> Vec<fxsound_preset::input::InputPreset> {
        use fxsound_preset::input::InputPreset;
        vec![
            InputPreset {
                name: "Loud".to_owned(),
                makeup_db: 9.0,
                chain: "podcast".to_owned(),
                ..InputPreset::default()
            },
            InputPreset {
                name: "Quiet".to_owned(),
                makeup_db: 0.0,
                highpass_hz: 75.0,
                gate: None,
                compressor: None,
                deesser: None,
                ..InputPreset::default()
            },
        ]
    }

    /// A settings file from a run that used the headphones with `Beta` at a 4 dB master gain, and
    /// the microphone — switched on — with `Loud`, last editing `edit`.
    fn saved_settings(edit: DeviceDirection) -> Settings {
        let mut settings = Settings::default();
        settings.output_preset = "Beta".to_owned();
        settings.input_preset = "Loud".to_owned();
        settings.master_gain = 4.0;
        settings.set_device_name(OUT, HEADPHONES);
        settings.set_device_name(IN, MIC);
        settings.set_lane_enabled(IN, true);
        settings.set_edit_direction(edit);
        settings
    }

    /// A start from `settings` against a fake engine, what the start-up said to it already taken.
    fn started_with(settings: Settings) -> (App, FakeEngine, tempfile::TempDir) {
        let engine = FakeEngine::new();
        let (store, dir) = music_store();
        let app = App::start_for_tests(settings, store, voices(&dir), &engine);
        let _ = engine.take_sent();
        let _ = engine.take_events();
        (app, engine, dir)
    }

    /// The engine's own words for a lane attached to `node_name`, or to nothing.
    fn attached(direction: DeviceDirection, node_name: Option<&str>) -> AudioToUi {
        AudioToUi::Attached {
            direction,
            node_name: node_name.map(str::to_owned),
        }
    }

    fn status(processing: bool) -> fxsound_core::AudioStatus {
        fxsound_core::AudioStatus {
            processing,
            ..fxsound_core::AudioStatus::default()
        }
    }

    fn selected(app: &App, direction: DeviceDirection) -> Option<&str> {
        app.state.device_for(direction).map(|d| d.name.as_str())
    }

    /// Where the device called `name` is in the list the window shows — in its priority list's
    /// order, not the order the engine sent.
    fn device_at(app: &App, name: &str) -> usize {
        app.state
            .devices
            .iter()
            .position(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
    }

    /// What the engine was sent, without the parameter snapshots and events.
    fn select_devices(sent: &[UiToAudio]) -> Vec<&UiToAudio> {
        sent.iter()
            .filter(|m| matches!(m, UiToAudio::SelectDevice { .. }))
            .collect()
    }

    #[test]
    fn a_start_tells_the_engine_what_the_settings_file_asks_before_anything_else() {
        let mut settings = saved_settings(OUT);
        settings.set_lane_enabled(OUT, false);
        settings.echo_cancel = true;
        let volume = TargetVolume {
            direction: OUT,
            target: HEADPHONES.to_owned(),
            channel_volumes: vec![0.5, 0.5],
            mute: false,
        };
        settings.device_volumes = vec![volume.clone()];

        let engine = FakeEngine::new();
        let (store, dir) = music_store();
        let _app = App::start_for_tests(settings, store, voices(&dir), &engine);
        let sent = engine.take_sent();
        assert!(
            matches!(sent[0], UiToAudio::SeedRememberedDefaults { .. }),
            "{sent:?}"
        );
        assert_eq!(sent[1], UiToAudio::SeedTargetVolumes(vec![volume]));
        assert_eq!(
            sent[2..4],
            [
                UiToAudio::SetDevicePriority {
                    direction: OUT,
                    names: Vec::new(),
                },
                UiToAudio::SetDevicePriority {
                    direction: IN,
                    names: Vec::new(),
                },
            ]
        );
        assert_eq!(sent[4], UiToAudio::DetachLane(OUT));
        assert_eq!(sent[5], UiToAudio::SetEchoCancel(true));
        // The microphone's saved preset names its chain, which only a message can carry.
        assert!(
            sent.contains(&UiToAudio::SetInputChain("podcast".to_owned())),
            "{sent:?}"
        );
        // No device yet: that waits for the list.
        assert!(select_devices(&sent).is_empty(), "{sent:?}");
    }

    #[test]
    fn a_start_publishes_each_lanes_snapshot_from_its_own_saved_preset() {
        for edit in [OUT, IN] {
            let engine = FakeEngine::new();
            let (store, dir) = music_store();
            let app = App::start_for_tests(saved_settings(edit), store, voices(&dir), &engine);
            let output = engine
                .params()
                .expect("the speakers' snapshot was published");
            let input = engine
                .input_params()
                .expect("the microphone's snapshot was published");
            assert!(
                (output.effect(Effect::Bass) - 0.6).abs() < 0.02,
                "editing {edit:?}: the speakers run Beta"
            );
            assert_eq!(output.master_gain_db, 4.0, "editing {edit:?}");
            assert_eq!(
                input.makeup_db, 9.0,
                "editing {edit:?}: the microphone runs Loud"
            );
            assert_eq!(app.state.direction, edit);
            // And each chain's history was cleared for the preset it now runs.
            let events = engine.take_events();
            for lane in [OUT, IN] {
                assert!(
                    events.contains(&(lane, DspEvent::ResetFilterState)),
                    "editing {edit:?}: {events:?}"
                );
            }
        }
    }

    #[test]
    fn after_start_up_each_lane_answers_for_its_preset_from_its_own_controls() {
        for edit in [OUT, IN] {
            let (mut app, _engine, _dir) = started_with(saved_settings(edit));
            let other = edit.other();
            assert!(
                app.lane_controls[lane_index(edit)].is_none(),
                "editing {edit:?}: the edit direction's controls are the window's"
            );
            assert!(
                app.lane_controls[lane_index(other)].is_some(),
                "editing {edit:?}: start-up entered the other lane too"
            );
            assert_eq!(
                app.lane_preset(OUT),
                Some(("Beta", false)),
                "editing {edit:?}"
            );
            assert_eq!(
                app.lane_preset(IN),
                Some(("Loud", false)),
                "editing {edit:?}"
            );
            // The lane's own controls answer, not the settings file's copy of its name.
            let answer = app.lane_preset(other).map(|(name, _)| name.to_owned());
            app.settings
                .set_preset_for_direction(other, "Somewhere else");
            assert_eq!(
                app.lane_preset(other).map(|(name, _)| name.to_owned()),
                answer,
                "editing {edit:?}"
            );
        }
    }

    #[test]
    fn an_unsaved_edit_goes_off_screen_with_its_lane_and_comes_back_with_it() {
        let (mut app, _engine, _dir) = started_with(saved_settings(IN));
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));

        app.handle(&[UiAction::SetEditDirection(OUT)]);
        assert_eq!(
            app.lane_preset(IN),
            Some(("Loud", true)),
            "reported from the microphone's own slot"
        );
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert!(app.lane_controls[lane_index(OUT)].is_none());

        app.handle(&[UiAction::SetEditDirection(IN)]);
        assert_eq!(app.state.eq_bands[0].boost_db, 6.0);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));
        assert!(app.lane_controls[lane_index(IN)].is_none());
        assert!(app.lane_controls[lane_index(OUT)].is_some());
    }

    #[test]
    fn the_list_at_start_up_announces_every_enabled_lanes_saved_device() {
        for edit in [OUT, IN] {
            let (mut app, engine, _dir) = started_with(saved_settings(edit));
            // The engine starts the speakers' lane on its own, by its device rules.
            engine.feed(attached(OUT, Some(SPEAKERS)));
            engine.feed(AudioToUi::Devices(two_lane_devices()));
            app.poll_audio();
            assert_eq!(
                select_devices(&engine.take_sent()),
                [&select(HEADPHONES, OUT), &select(MIC, IN)],
                "editing {edit:?}"
            );
            // Each lane shows the device it has been asked for, which is where it is going: the
            // speakers' lane is still on the speakers, and the microphone's on nothing, until the
            // engine answers.
            assert_eq!(selected(&app, OUT), Some(HEADPHONES));
            assert_eq!(app.attached(OUT), Some(SPEAKERS));
            assert_eq!(selected(&app, IN), Some(MIC));

            engine.feed(attached(OUT, Some(HEADPHONES)));
            engine.feed(attached(IN, Some(MIC)));
            engine.feed(AudioToUi::Devices(two_lane_devices()));
            app.poll_audio();
            assert_eq!(selected(&app, OUT), Some(HEADPHONES));
            assert_eq!(selected(&app, IN), Some(MIC));
            assert!(
                select_devices(&engine.take_sent()).is_empty(),
                "each saved device is announced once"
            );
        }
    }

    #[test]
    fn a_microphone_left_off_is_neither_announced_nor_shown_at_start_up() {
        let mut settings = saved_settings(IN);
        settings.set_lane_enabled(IN, false);
        let (mut app, engine, _dir) = started_with(settings);
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert_eq!(
            select_devices(&engine.take_sent()),
            [&select(HEADPHONES, OUT)]
        );
        assert_eq!(app.state.selected_input, None);
    }

    #[test]
    fn a_start_with_the_speakers_off_shows_off_even_if_the_engine_attached_them_first() {
        let mut settings = saved_settings(OUT);
        settings.set_lane_enabled(OUT, false);
        let (mut app, engine, _dir) = started_with(settings);
        // Attached by the device rules before the start-up's DetachLane reached the engine.
        engine.feed(attached(OUT, Some(SPEAKERS)));
        engine.feed(AudioToUi::Status {
            direction: OUT,
            status: status(true),
        });
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert_eq!(app.state.selected_output, None);
        assert!(!app.state.output_active);
        assert!(
            !select_devices(&engine.take_sent())
                .iter()
                .any(|m| matches!(m, UiToAudio::SelectDevice { direction: OUT, .. })),
            "the speakers stay off"
        );
    }

    #[test]
    fn the_combo_follows_the_engine_when_its_rules_move_a_lane() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        let _ = engine.take_sent();

        // The headphones are unplugged and the engine moves the lane to the speakers.
        engine.feed(attached(OUT, Some(SPEAKERS)));
        engine.feed(AudioToUi::Devices(vec![
            device(SPEAKERS, OUT, true),
            device(MIC, IN, true),
        ]));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        assert_eq!(
            app.settings.device_name(OUT),
            HEADPHONES,
            "the user's choice is kept for when they come back"
        );

        // They come back: the saved choice is announced again, and the engine's answer shown.
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert_eq!(
            select_devices(&engine.take_sent()),
            [&select(HEADPHONES, OUT)]
        );
        engine.feed(attached(OUT, Some(HEADPHONES)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
    }

    #[test]
    fn a_pick_is_shown_at_once_and_kept_until_the_engine_answers_it() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        engine.feed(attached(IN, Some(MIC)));
        app.poll_audio();
        let _ = engine.take_sent();

        app.handle(&[UiAction::DetachInput]);
        engine.feed(attached(IN, None));
        app.poll_audio();
        assert_eq!(app.state.selected_input, None);
        assert_eq!(engine.take_sent(), [UiToAudio::DetachLane(IN)]);

        app.handle(&[UiAction::SelectInput(2)]);
        assert_eq!(selected(&app, IN), Some(MIC), "shown in the same frame");
        assert_eq!(engine.take_sent(), [select(MIC, IN)]);
        // A list the engine sent before it got to the pick does not take it back.
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert_eq!(selected(&app, IN), Some(MIC));
        assert_eq!(app.attached(IN), None);

        engine.feed(attached(IN, Some(MIC)));
        app.poll_audio();
        assert_eq!(selected(&app, IN), Some(MIC));
        assert_eq!(app.attached(IN), Some(MIC));
    }

    #[test]
    fn a_pick_on_a_lane_that_is_attached_elsewhere_is_kept_until_that_lane_answers() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        assert_eq!(
            selected(&app, IN),
            Some(MIC),
            "the saved microphone, announced and not answered yet"
        );
        let _ = engine.take_sent();

        app.handle(&[UiAction::SelectOutput(device_at(&app, SPEAKERS))]);
        assert_eq!(
            selected(&app, OUT),
            Some(SPEAKERS),
            "shown in the same frame"
        );
        assert_eq!(engine.take_sent(), [select(SPEAKERS, OUT)]);

        // Everything that redraws both combos before the engine gets to the pick leaves it shown:
        // a device list (FxSound's own sink becoming the default clears the speakers' flag), an
        // error about the other lane, and the other lane's attachment.
        let mut list = two_lane_devices();
        for device in list.iter_mut().filter(|device| device.direction == OUT) {
            device.is_default = false;
        }
        engine.feed(AudioToUi::Devices(list));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(SPEAKERS), "after a device list");

        engine.feed(AudioToUi::Error {
            direction: Some(IN),
            message: "audio device is unavailable".to_owned(),
        });
        app.poll_audio();
        assert_eq!(
            selected(&app, OUT),
            Some(SPEAKERS),
            "after the microphone's error"
        );
        assert_eq!(
            app.state.selected_input, None,
            "the microphone's own request ended with its error"
        );

        engine.feed(attached(IN, Some(MIC)));
        app.poll_audio();
        assert_eq!(
            selected(&app, OUT),
            Some(SPEAKERS),
            "after the microphone's attachment"
        );
        assert_eq!(selected(&app, IN), Some(MIC));
        assert_eq!(
            app.attached(OUT),
            Some(HEADPHONES),
            "the sound is still on the headphones until the engine says otherwise"
        );

        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        assert_eq!(app.attached(OUT), Some(SPEAKERS));
        assert!(
            select_devices(&engine.take_sent()).is_empty(),
            "nothing was asked twice"
        );
    }

    #[test]
    fn a_pick_the_engine_could_not_attach_is_not_shown_as_attached() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        engine.feed(attached(OUT, Some(SPEAKERS)));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        engine.feed(attached(OUT, Some(HEADPHONES)));
        app.poll_audio();

        app.handle(&[UiAction::SelectOutput(device_at(&app, SPEAKERS))]);
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        engine.feed(AudioToUi::Error {
            direction: Some(OUT),
            message: "audio device is unavailable".to_owned(),
        });
        app.poll_audio();
        assert_eq!(
            selected(&app, OUT),
            Some(HEADPHONES),
            "the sound is still going to the headphones"
        );
        assert_eq!(
            app.state.notification.as_deref(),
            Some("audio device is unavailable")
        );
    }

    #[test]
    fn switching_a_lane_off_keeps_the_settings_the_engine_and_the_window_in_step() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        engine.feed(attached(IN, Some(MIC)));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        engine.feed(AudioToUi::Status {
            direction: IN,
            status: status(true),
        });
        app.poll_audio();
        assert!(app.state.input_active);
        let _ = engine.take_sent();

        app.handle(&[UiAction::DetachInput]);
        assert_eq!(engine.take_sent(), [UiToAudio::DetachLane(IN)]);
        assert!(!app.settings.lane_enabled(IN));
        assert_eq!(app.state.selected_input, None);
        assert!(!app.state.input_active);

        // A status that was already on its way when the lane was switched off…
        engine.feed(AudioToUi::Status {
            direction: IN,
            status: status(true),
        });
        app.poll_audio();
        assert!(!app.state.input_active, "a lane switched off is not active");
        // …then the engine's answer.
        engine.feed(attached(IN, None));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert!(!app.state.input_active);
        assert_eq!(app.state.selected_input, None);
        assert_eq!(app.attached(IN), None);
        assert!(
            engine.take_sent().is_empty(),
            "nothing attaches the microphone again"
        );
        // The speakers were never touched.
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        assert!(app.settings.lane_enabled(OUT));

        // Picking it again switches the lane back on, and is said to the engine again.
        app.handle(&[UiAction::SelectInput(2)]);
        assert_eq!(engine.take_sent(), [select(MIC, IN)]);
        assert!(app.settings.lane_enabled(IN));
    }

    #[test]
    fn switching_the_speakers_off_leaves_the_microphone_running() {
        let (mut app, engine, _dir) = started_with(saved_settings(IN));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        engine.feed(attached(IN, Some(MIC)));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        let _ = engine.take_sent();

        app.handle(&[UiAction::DetachOutput]);
        engine.feed(attached(OUT, None));
        app.poll_audio();
        assert_eq!(engine.take_sent(), [UiToAudio::DetachLane(OUT)]);
        assert_eq!(app.state.selected_output, None);
        assert_eq!(selected(&app, IN), Some(MIC));
        assert_eq!(app.state.direction, IN);
    }

    #[test]
    fn each_lanes_status_sets_its_own_activity_and_a_lane_that_stops_is_cleared() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        engine.feed(attached(IN, Some(MIC)));
        for lane in [OUT, IN] {
            engine.feed(AudioToUi::Status {
                direction: lane,
                status: status(true),
            });
        }
        app.poll_audio();
        assert!(app.state.output_active && app.state.input_active);

        engine.feed(AudioToUi::Status {
            direction: OUT,
            status: status(false),
        });
        app.poll_audio();
        assert!(!app.state.output_active);
        assert!(app.state.input_active, "the microphone said nothing");

        engine.feed(attached(IN, None));
        app.poll_audio();
        assert!(!app.state.input_active, "a lane with no nodes stopped");
    }

    #[test]
    fn a_lost_connection_leaves_no_lane_active() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        for lane in [OUT, IN] {
            engine.feed(attached(
                lane,
                Some(if lane == OUT { SPEAKERS } else { MIC }),
            ));
            engine.feed(AudioToUi::Status {
                direction: lane,
                status: status(true),
            });
        }
        engine.feed(AudioToUi::Disconnected {
            reason: "the server went away".to_owned(),
        });
        app.poll_audio();
        assert!(!app.state.output_active);
        assert!(!app.state.input_active);
    }

    #[test]
    fn a_warning_from_the_engine_is_shown_as_a_notice() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        let message = "Using this headset's microphone switches it to call quality";
        engine.feed(AudioToUi::Warning {
            direction: None,
            message: message.to_owned(),
        });
        app.poll_audio();
        assert_eq!(app.state.notification.as_deref(), Some(message));
        assert!(
            app.state.notice_clock.is_some(),
            "raised through its clock, so it expires"
        );
    }

    #[test]
    fn a_volume_the_engine_reports_is_remembered_and_saved_a_moment_later() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        app.handle(&[]);
        let volume = |level: f32| TargetVolume {
            direction: OUT,
            target: HEADPHONES.to_owned(),
            channel_volumes: vec![level, level],
            mute: false,
        };

        engine.feed(AudioToUi::TargetVolume(volume(0.4)));
        app.poll_audio();
        assert_eq!(
            app.settings.target_volume(OUT, HEADPHONES),
            Some(&volume(0.4))
        );
        assert!(
            !app.settings_dirty,
            "not written on the first step of a drag"
        );
        let due = app.volume_save_due.expect("a write is due");

        // More steps of the same drag: remembered, and still one write, counted from the first.
        engine.feed(AudioToUi::TargetVolume(volume(0.3)));
        engine.feed(AudioToUi::TargetVolume(volume(0.2)));
        app.poll_audio();
        assert_eq!(
            app.settings.target_volume(OUT, HEADPHONES),
            Some(&volume(0.2))
        );
        assert_eq!(app.volume_save_due, Some(due));

        app.volume_save_due = Instant::now().checked_sub(std::time::Duration::from_millis(1));
        app.poll_audio();
        assert!(app.settings_dirty, "due: the next flush writes it");
        assert_eq!(app.volume_save_due, None);
        app.handle(&[]);

        // The same volume reported again is nothing new.
        engine.feed(AudioToUi::TargetVolume(volume(0.2)));
        app.poll_audio();
        assert_eq!(app.volume_save_due, None);

        // And the next start hands it to the engine.
        assert!(
            startup_messages(app.settings())
                .contains(&UiToAudio::SeedTargetVolumes(vec![volume(0.2)]))
        );
    }

    #[test]
    fn a_volume_reported_while_the_settings_pane_is_open_outlives_what_the_pane_does() {
        // The pane edits a copy of the settings; what it changes is written back field by field,
        // so a volume the engine reported meanwhile is merged, not overwritten by the copy.
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        let mut pane = app.settings_state();
        let volume = TargetVolume {
            direction: OUT,
            target: HEADPHONES.to_owned(),
            channel_volumes: vec![0.3, 0.3],
            mute: false,
        };
        engine.feed(AudioToUi::TargetVolume(volume.clone()));
        app.poll_audio();
        app.handle_settings(&SettingsAction::SetHideHelpTips(true), &mut pane);
        app.handle_settings(&SettingsAction::MoveDeviceDown(0), &mut pane);
        app.handle_settings(&SettingsAction::SetFollowSystemDefault(true), &mut pane);
        app.refresh_settings_state(&mut pane);
        assert_eq!(app.settings.target_volume(OUT, HEADPHONES), Some(&volume));
        assert!(
            startup_messages(&app.settings).contains(&UiToAudio::SeedTargetVolumes(vec![volume]))
        );
    }

    #[test]
    fn the_strip_reads_the_microphone_lane_and_the_picture_the_edited_lane() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        let mut music = Meters {
            active: true,
            sample_rate: 48_000,
            ..Meters::default()
        };
        music.spectrum[3] = 0.7;
        let mut voice = Meters {
            active: true,
            sample_rate: 16_000,
            noise_floor_db: -48.0,
            voice_probability: 0.8,
            gate_reduction_db: 6.0,
            denoise_reduction_db: 12.0,
            ..Meters::default()
        };
        voice.spectrum[3] = 0.2;
        engine.set_meters(OUT, music);
        engine.set_meters(IN, voice);

        app.poll_audio();
        assert_eq!(app.state.spectrum, music.spectrum);
        assert_eq!(app.state.sample_rate, 48_000);
        assert_eq!(app.state.noise_floor_db, -48.0);
        assert_eq!(app.state.voice_probability, 0.8);
        assert_eq!(app.state.gate_reduction_db, 6.0);
        assert_eq!(app.state.denoise_reduction_db, 12.0);

        app.handle(&[UiAction::SetEditDirection(IN)]);
        app.poll_audio();
        assert_eq!(app.state.spectrum, voice.spectrum);
        assert_eq!(app.state.sample_rate, 16_000);
        assert_eq!(app.state.noise_floor_db, -48.0);
    }

    /// The microphone lane on screen with `Loud` (9 dB of makeup), the speakers at their saved
    /// 4 dB master gain.
    fn editing_the_microphone() -> (App, FakeEngine, tempfile::TempDir) {
        let (mut app, engine, dir) = started_with(saved_settings(OUT));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        app.handle(&[UiAction::SelectInput(2)]);
        assert_eq!(app.state.direction, IN);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Loud"));
        (app, engine, dir)
    }

    #[test]
    fn a_voice_presets_makeup_never_reaches_the_speakers_master_gain() {
        let (mut app, engine, _dir) = editing_the_microphone();
        assert_eq!(app.state.master_gain_db, 9.0, "the window shows the voice");
        let published = |engine: &FakeEngine| engine.params().expect("published").master_gain_db;
        assert_eq!(published(&engine), 4.0);

        app.handle(&[UiAction::SetMasterGain(12.0), UiAction::SelectPreset(0)]);
        assert_eq!(
            engine.input_params().expect("published").makeup_db,
            9.0,
            "Loud picked again"
        );
        app.handle(&[UiAction::SetMasterGain(12.0)]);
        assert_eq!(engine.input_params().expect("published").makeup_db, 12.0);
        assert_eq!(published(&engine), 4.0, "the speakers kept their gain");
        assert_eq!(
            app.settings.master_gain, 4.0,
            "and so did the settings file"
        );

        app.handle(&[UiAction::SetEditDirection(OUT)]);
        assert_eq!(app.state.master_gain_db, 4.0, "the window shows the music");
        assert_eq!(published(&engine), 4.0);
        assert_eq!(engine.input_params().expect("published").makeup_db, 12.0);
    }

    #[test]
    fn the_speakers_master_gain_never_reaches_the_voices_makeup() {
        let (mut app, engine, _dir) = editing_the_microphone();
        let loud = engine.input_params().expect("published");
        app.handle(&[UiAction::SetEditDirection(OUT)]);
        app.handle(&[UiAction::SetMasterGain(-6.0), UiAction::SetBandGain(2, 5.0)]);
        assert_eq!(engine.params().expect("published").master_gain_db, -6.0);
        let voice = engine.input_params().expect("published");
        assert_eq!(voice.makeup_db, 9.0, "the voice kept Loud's makeup");
        assert_eq!(voice, loud, "and everything else of Loud's");

        app.handle(&[UiAction::SetEditDirection(IN)]);
        assert_eq!(app.state.master_gain_db, 9.0);
        assert_eq!(app.state.eq_bands[2].boost_db, loud.bands().1[2]);
        assert_eq!(engine.params().expect("published").master_gain_db, -6.0);
    }

    #[test]
    fn the_power_switch_reaches_both_snapshots_and_clears_both_chains_coming_back() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        app.handle(&[UiAction::TogglePower]);
        assert!(!engine.params().expect("published").power);
        assert!(!engine.input_params().expect("published").power);
        assert!(
            engine.take_events().is_empty(),
            "nothing to clear going off"
        );

        app.handle(&[UiAction::TogglePower]);
        assert!(engine.params().expect("published").power);
        assert!(engine.input_params().expect("published").power);
        assert_eq!(
            engine.take_events(),
            [
                (OUT, DspEvent::ResetFilterState),
                (IN, DspEvent::ResetFilterState)
            ]
        );
    }

    #[test]
    fn a_preset_clears_the_history_of_its_own_lane_only() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        app.handle(&[UiAction::SelectPreset(0)]);
        assert_eq!(engine.take_events(), [(OUT, DspEvent::ResetFilterState)]);

        app.handle(&[UiAction::SetEditDirection(IN), UiAction::SelectPreset(1)]);
        assert_eq!(engine.take_events(), [(IN, DspEvent::ResetFilterState)]);
        assert!(
            engine.take_sent().contains(&UiToAudio::SetInputChain(
                fxsound_preset::input::DEFAULT_CHAIN.to_owned()
            )),
            "Quiet runs the default chain, and the engine is told"
        );
    }

    #[test]
    fn a_new_band_count_clears_the_history_of_the_edited_lane_only() {
        let (mut app, engine, _dir) = started_with(saved_settings(IN));
        app.handle(&[UiAction::SetBandCount(5)]);
        assert_eq!(engine.take_events(), [(IN, DspEvent::ResetFilterState)]);
        assert_eq!(engine.params().expect("published").bands().0.len(), 10);
    }

    #[test]
    fn echo_cancellation_ticked_in_the_pane_is_asked_of_the_engine_at_once() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        let mut pane = app.settings_state();
        app.handle_settings(&SettingsAction::SetEchoCancel(true), &mut pane);
        assert_eq!(engine.take_sent(), [UiToAudio::SetEchoCancel(true)]);
        engine.feed(AudioToUi::EchoCancel {
            running: false,
            detail: "no libspa-aec-webrtc".to_owned(),
        });
        app.poll_audio();
        app.refresh_settings_state(&mut pane);
        assert_eq!(
            pane.echo_cancel_status().as_deref(),
            Some("unavailable · no libspa-aec-webrtc")
        );
        app.handle_settings(&SettingsAction::SetEchoCancel(false), &mut pane);
        assert_eq!(engine.take_sent(), [UiToAudio::SetEchoCancel(false)]);
    }

    #[test]
    fn shutting_down_stops_the_engine() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        app.shutdown();
        assert!(engine.is_shut_down());
        assert!(!app.has_audio());
    }

    /// The music autosaves on disk in `dir`'s store (see [`music_store`]), by file name.
    fn music_autosaves(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("user").join("AutoSave"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The next run on `dir`'s preset directories, last editing `edit`.
    fn started_again(dir: &tempfile::TempDir, edit: DeviceDirection) -> App {
        let mut store =
            PresetStore::with_dirs(vec![dir.path().join("factory")], dir.path().join("user"));
        store.rescan();
        App::start_for_tests(saved_settings(edit), store, voices(dir), &FakeEngine::new())
    }

    #[test]
    fn a_quit_while_editing_the_microphone_stashes_the_speakers_unsaved_edits() {
        let (mut app, _engine, dir) = started_with(saved_settings(OUT));
        app.handle(&[UiAction::SetEffect(Effect::Bass, 7.0)]);
        app.handle(&[UiAction::SetEditDirection(IN)]);
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        assert_eq!(app.lane_preset(OUT), Some(("Beta", true)));
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));

        app.shutdown();
        assert_eq!(
            music_autosaves(&dir),
            ["Beta.fac"],
            "the speakers' edits, and nothing filed under the voice preset's name"
        );
        let stashed = fxsound_preset::load(&dir.path().join("user/AutoSave/Beta.fac"))
            .expect("the autosave reads back");
        assert_eq!(
            stashed.eq_bands[0].boost_db, 0.0,
            "the speakers' own equalizer, not the microphone's"
        );

        let next = started_again(&dir, OUT);
        assert_eq!(next.lane_preset(OUT), Some(("Beta", true)));
        assert!(
            (next.state.effect(Effect::Bass) - 7.0).abs() < 0.1,
            "the edit came back: bass at {}",
            next.state.effect(Effect::Bass)
        );
        assert_eq!(
            started_again(&dir, IN).lane_preset(OUT),
            Some(("Beta", true)),
            "and comes back with the lane off screen too"
        );
    }

    #[test]
    fn a_quit_while_editing_the_speakers_stashes_their_edits_and_not_the_microphones() {
        let (mut app, _engine, dir) = started_with(saved_settings(IN));
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        app.handle(&[UiAction::SetEditDirection(OUT)]);
        app.handle(&[UiAction::SetEffect(Effect::Bass, 7.0)]);
        app.shutdown();
        assert_eq!(music_autosaves(&dir), ["Beta.fac"]);
    }

    #[test]
    fn a_voice_presets_unsaved_edits_never_reach_the_music_autosave() {
        let (mut app, _engine, dir) = started_with(saved_settings(IN));
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        app.shutdown();
        assert_eq!(music_autosaves(&dir), Vec::<String>::new());
    }

    // ---- writable voice presets (0.4.0 design §1.4, §11) ----------------------------------------

    /// Every `.fac` under the speakers' user directory, its AutoSave included, by path relative
    /// to it.
    fn music_files(dir: &tempfile::TempDir) -> Vec<String> {
        let root = dir.path().join("user");
        let mut found = Vec::new();
        let mut pending = vec![root.clone()];
        while let Some(at) = pending.pop() {
            for entry in std::fs::read_dir(&at).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|e| e == "fac") {
                    let relative = path.strip_prefix(&root).expect("under the root");
                    found.push(relative.display().to_string());
                }
            }
        }
        found.sort();
        found
    }

    /// The voice presets in `sub` of the user's voice directory (`user/Input`), by file name.
    fn voice_files(dir: &tempfile::TempDir, sub: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("user/Input").join(sub))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "toml"))
            .filter_map(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names
    }

    /// A user voice preset as it is on disk.
    fn saved_voice(dir: &tempfile::TempDir, file: &str) -> InputPreset {
        InputPreset::load(&dir.path().join("user/Input").join(file)).expect("a voice preset")
    }

    /// Where `name` is in the list the window shows.
    fn at(app: &App, name: &str) -> usize {
        app.state
            .presets
            .iter()
            .position(|p| p.name == name)
            .unwrap_or_else(|| panic!("{name} is listed"))
    }

    /// The entry the window shows for `name`.
    fn entry<'a>(app: &'a App, name: &str) -> &'a PresetEntry {
        &app.state.presets[at(app, name)]
    }

    /// Loud as shipped, for comparing what a save kept.
    fn loud() -> InputPreset {
        voice_list().remove(0)
    }

    #[test]
    fn a_voice_preset_is_never_filed_as_a_fac_by_a_save_or_an_autosave() {
        // 0.3.0 built a `.fac` from the window whichever lane it showed: Save on a microphone
        // filed the voice preset's equalizer as a music preset named after it, and the autosaves
        // on switching away and on the way out did the same in the music AutoSave — where a voice
        // preset named like a `.fac` overwrote that music preset's unsaved edits.
        let (mut app, _engine, dir) = editing_the_microphone();
        assert!(music_files(&dir).is_empty());

        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        // The speakers' preset's own name, free in the voice list.
        app.handle(&[UiAction::SavePresetAs("Beta".into())]);
        app.handle(&[UiAction::SetBandGain(1, 4.0), UiAction::SavePreset]);
        app.handle(&[UiAction::SetBandGain(2, 3.0)]);
        app.handle(&[UiAction::SelectPreset(at(&app, "Quiet"))]);
        app.handle(&[UiAction::SetBandGain(3, 2.0)]);
        app.shutdown();

        assert_eq!(
            music_files(&dir),
            Vec::<String>::new(),
            "no .fac anywhere, saved or stashed"
        );
        assert_eq!(voice_files(&dir, ""), ["Beta.toml"]);
        assert_eq!(voice_files(&dir, "AutoSave"), ["Beta.toml", "Quiet.toml"]);
        let beta = saved_voice(&dir, "Beta.toml");
        assert_eq!(&beta.eq.gains_db[..2], [6.0, 4.0]);

        // The speakers' Beta is still the factory one, with nothing waiting to be saved.
        let next = started_again(&dir, OUT);
        assert_eq!(next.lane_preset(OUT), Some(("Beta", false)));
        assert!((next.params().effect(Effect::Bass) - 0.6).abs() < 0.02);
    }

    #[test]
    fn save_new_preset_on_the_microphone_files_a_user_voice_preset_and_selects_it() {
        let (mut app, _engine, dir) = editing_the_microphone();
        app.handle(&[UiAction::SetBandGain(0, 6.0), UiAction::SetMasterGain(3.0)]);
        assert!(entry(&app, "Loud").modified);

        app.handle(&[UiAction::SavePresetAs("Mine".into())]);

        let mine = app.state.preset().expect("the new preset is selected");
        assert_eq!(mine.name, "Mine");
        assert!(!mine.factory, "the user's own");
        assert!(!mine.modified, "and saved");
        assert!(!entry(&app, "Loud").modified, "the edits went to Mine");
        assert!(entry(&app, "Loud").factory);
        assert_eq!(app.settings.input_preset, "Mine");
        assert_eq!(
            app.settings.output_preset, "Beta",
            "the speakers' is their own"
        );
        assert_eq!(app.settings.preset_for_device(MIC, IN), Some("Mine"));
        assert_eq!(app.settings.preset_for_device(MIC, OUT), None);
        assert_eq!(
            app.state.notification.as_deref(),
            Some("New preset Mine is saved.")
        );

        let saved = saved_voice(&dir, "Mine.toml");
        assert_eq!(saved.name, "Mine");
        assert_eq!(saved.eq.gains_db[0], 6.0);
        assert_eq!(
            saved.makeup_db, 3.0,
            "the makeup is the voice's output gain"
        );
        // Everything the window has no control for is Loud's, as Loud had it.
        let loud = loud();
        assert_eq!(saved.chain, "podcast");
        assert_eq!(saved.gate, loud.gate);
        assert_eq!(saved.compressor, loud.compressor);
        assert_eq!(saved.deesser, loud.deesser);
        assert_eq!(saved.highpass_hz, loud.highpass_hz);
        assert_eq!(saved.ceiling_db, loud.ceiling_db);
        assert_eq!(voice_files(&dir, "AutoSave"), Vec::<String>::new());
    }

    #[test]
    fn a_user_voice_preset_is_listed_as_the_users_at_the_next_start() {
        let (mut app, _engine, dir) = editing_the_microphone();
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        app.handle(&[UiAction::SavePresetAs("Mine".into())]);
        app.shutdown();

        let mut settings = saved_settings(IN);
        settings.input_preset = "Mine".to_owned();
        let mut store =
            PresetStore::with_dirs(vec![dir.path().join("factory")], dir.path().join("user"));
        store.rescan();
        let next = App::start_for_tests(settings, store, voices(&dir), &FakeEngine::new());
        assert_eq!(next.lane_preset(IN), Some(("Mine", false)));
        assert!(!entry(&next, "Mine").factory);
        assert!(entry(&next, "Loud").factory);
        assert!(next.lane_has_preset(IN, "Mine"));
        assert!(!next.lane_has_preset(OUT, "Mine"));
        assert_eq!(next.input_params().bands().1[0], 6.0);
        assert_eq!(next.input_chain(), "podcast");
    }

    #[test]
    fn overwriting_a_user_voice_preset_saves_the_edits_over_it_and_keeps_the_last_version() {
        let (mut app, _engine, dir) = editing_the_microphone();
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        app.handle(&[UiAction::SavePresetAs("Mine".into())]);
        let listed = app.state.presets.len();
        app.handle(&[UiAction::SetBandGain(0, -4.0)]);
        assert!(entry(&app, "Mine").modified);

        app.handle(&[UiAction::SavePreset]);

        assert!(!entry(&app, "Mine").modified);
        assert_eq!(
            app.state.presets.len(),
            listed,
            "the same preset, not a new one"
        );
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Mine"));
        assert_eq!(saved_voice(&dir, "Mine.toml").eq.gains_db[0], -4.0);
        let backup = InputPreset::load(&dir.path().join("user/Input/Mine.toml.bak"))
            .expect("the version it replaced");
        assert_eq!(backup.eq.gains_db[0], 6.0);
    }

    #[test]
    fn a_voice_preset_saved_with_the_equalizer_off_comes_back_off() {
        let (mut app, engine, dir) = editing_the_microphone();
        app.handle(&[UiAction::SetEqEnabled(false)]);
        assert!(entry(&app, "Loud").modified, "the switch is an edit");
        app.handle(&[UiAction::SavePresetAs("Dry".into())]);
        assert!(!saved_voice(&dir, "Dry.toml").eq.enabled);

        app.handle(&[UiAction::SelectPreset(at(&app, "Quiet"))]);
        assert!(app.state.eq_on);
        assert!(engine.input_params().expect("published").eq_on);
        app.handle(&[UiAction::SelectPreset(at(&app, "Dry"))]);
        assert!(!app.state.eq_on);
        assert!(!engine.input_params().expect("published").eq_on);
    }

    #[test]
    fn switching_voice_presets_stashes_the_edits_and_brings_them_back_marked() {
        let (mut app, engine, dir) = editing_the_microphone();
        app.handle(&[UiAction::SetBandGain(4, 5.0)]);
        app.handle(&[UiAction::SelectPreset(at(&app, "Quiet"))]);

        assert_eq!(voice_files(&dir, "AutoSave"), ["Loud.toml"]);
        assert!(entry(&app, "Loud").modified, "the list still says so");
        assert!(!entry(&app, "Quiet").modified);
        assert_eq!(app.lane_preset(IN), Some(("Quiet", false)));

        app.handle(&[UiAction::SelectPreset(at(&app, "Loud"))]);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));
        assert_eq!(app.state.eq_bands[4].boost_db, 5.0);
        assert_eq!(engine.input_params().expect("published").bands().1[4], 5.0);
        assert_eq!(
            voice_files(&dir, ""),
            Vec::<String>::new(),
            "a stash is not a save"
        );
    }

    #[test]
    fn undo_on_the_microphone_drops_the_stash_and_loads_the_voice_as_saved() {
        let (mut app, engine, dir) = editing_the_microphone();
        let saved_band = app.state.eq_bands[4].boost_db;
        app.handle(&[UiAction::SetBandGain(4, 5.0), UiAction::SetMasterGain(12.0)]);
        app.handle(&[UiAction::SelectPreset(at(&app, "Quiet"))]);
        app.handle(&[UiAction::SelectPreset(at(&app, "Loud"))]);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));

        app.handle(&[UiAction::UndoPresetChanges]);

        assert_eq!(app.lane_preset(IN), Some(("Loud", false)));
        assert_eq!(voice_files(&dir, "AutoSave"), Vec::<String>::new());
        assert_eq!(app.state.eq_bands[4].boost_db, saved_band);
        assert_eq!(app.state.master_gain_db, 9.0);
        let published = engine.input_params().expect("published");
        assert_eq!(published.makeup_db, 9.0);
        assert_eq!(published.bands().1[4], saved_band);
    }

    #[test]
    fn deleting_a_user_voice_preset_removes_its_file_and_a_shipped_voice_refuses() {
        let (mut app, _engine, dir) = editing_the_microphone();
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        app.handle(&[UiAction::SavePresetAs("Mine".into())]);
        assert_eq!(voice_files(&dir, ""), ["Mine.toml"]);

        app.handle(&[UiAction::DeletePreset]);
        assert_eq!(voice_files(&dir, ""), Vec::<String>::new());
        assert_eq!(names(&app), ["Loud", "Quiet"]);
        assert!(
            app.state.preset().is_some(),
            "the selection moved to a listed voice"
        );
        assert_eq!(
            app.state.notification.as_deref(),
            Some("Preset Mine is deleted.")
        );

        app.handle(&[UiAction::SelectPreset(at(&app, "Loud"))]);
        app.handle(&[UiAction::DeletePreset]);
        assert_eq!(names(&app), ["Loud", "Quiet"]);
        assert!(dir.path().join("voice-factory/Loud.toml").is_file());
        assert_eq!(
            app.state.notification.as_deref(),
            Some("Factory presets cannot be deleted")
        );
    }

    #[test]
    fn renaming_a_user_voice_preset_moves_its_file_and_what_named_it() {
        let (mut app, _engine, dir) = editing_the_microphone();
        // A speaker that remembers a `.fac` of the same name: another lane's name, left alone.
        app.settings
            .remember_device_preset(SPEAKERS, "Speakers", "Mine", "", OUT);
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        app.handle(&[UiAction::SavePresetAs("Mine".into())]);
        assert_eq!(app.settings.preset_for_device(MIC, IN), Some("Mine"));

        app.rename_preset("Yours");

        assert_eq!(voice_files(&dir, ""), ["Yours.toml"]);
        assert_eq!(saved_voice(&dir, "Yours.toml").name, "Yours");
        assert_eq!(saved_voice(&dir, "Yours.toml").eq.gains_db[0], 6.0);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Yours"));
        assert!(!app.state.preset().is_some_and(|p| p.modified));
        assert_eq!(app.settings.input_preset, "Yours");
        assert_eq!(app.settings.output_preset, "Beta");
        assert_eq!(app.settings.preset_for_device(MIC, IN), Some("Yours"));
        assert_eq!(app.settings.preset_for_device(SPEAKERS, OUT), Some("Mine"));
        assert_eq!(
            app.loaded_voice.as_ref().map(|p| p.name.as_str()),
            Some("Yours")
        );
        assert!(music_files(&dir).is_empty());

        // A shipped voice refuses, as a shipped `.fac` does.
        app.handle(&[UiAction::SelectPreset(at(&app, "Quiet"))]);
        app.rename_preset("Hushed");
        assert_eq!(names(&app), ["Loud", "Quiet", "Yours"]);
        assert_eq!(
            app.state.notification.as_deref(),
            Some("Factory presets cannot be renamed")
        );
    }

    #[test]
    fn importing_takes_the_edit_directions_kind_of_file_and_leaves_the_other() {
        let (mut app, _engine, dir) = editing_the_microphone();
        let incoming = dir.path().join("incoming");
        std::fs::create_dir_all(&incoming).unwrap();
        let podcast = InputPreset {
            name: "Podcast Mic".to_owned(),
            ..InputPreset::default()
        };
        podcast.save(&incoming.join("Podcast Mic.toml")).unwrap();
        // Taken, case-insensitively, by the shipped Quiet.
        podcast.save(&incoming.join("quiet.toml")).unwrap();
        let rock = Preset {
            name: "Rock".to_owned(),
            ..Preset::default()
        };
        fxsound_preset::save(&rock, &incoming.join("Rock.fac")).unwrap();

        let mut state = ImportState {
            folder: Some(incoming.clone()),
            ..ImportState::default()
        };
        app.handle_import(&PresetsAction::Import, &mut state);
        let summary = state.summary.clone().expect("import ran");
        assert_eq!(summary.imported, ["Podcast Mic"]);
        assert_eq!(summary.skipped, ["quiet"]);
        assert_eq!(voice_files(&dir, ""), ["Podcast Mic.toml"]);
        assert!(music_files(&dir).is_empty(), "the .fac is the speakers'");
        assert!(!entry(&app, "Podcast Mic").factory);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Loud"));

        // On the speakers the same folder gives the `.fac` and leaves the voices alone.
        app.handle(&[UiAction::SetEditDirection(OUT)]);
        let mut state = ImportState {
            folder: Some(incoming),
            ..ImportState::default()
        };
        app.handle_import(&PresetsAction::Import, &mut state);
        assert_eq!(state.summary.expect("import ran").imported, ["Rock"]);
        assert_eq!(music_files(&dir), ["Rock.fac"]);
        assert_eq!(voice_files(&dir, ""), ["Podcast Mic.toml"]);
    }

    #[test]
    fn exporting_on_the_microphone_writes_voice_files_as_last_saved() {
        let (mut app, _engine, dir) = editing_the_microphone();
        app.export_dir = dir.path().join("export");
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        app.handle(&[UiAction::SavePresetAs("Mine".into())]);
        // An edit, stashed by the switch away: not what "Mine" says until it is saved.
        app.handle(&[UiAction::SetBandGain(0, 2.0)]);
        app.handle(&[UiAction::SelectPreset(at(&app, "Quiet"))]);
        assert!(entry(&app, "Mine").modified);

        let mut state = ExportState {
            presets: app.state.presets.iter().map(|p| p.name.clone()).collect(),
            selected: [at(&app, "Loud"), at(&app, "Mine")].into_iter().collect(),
            ..ExportState::default()
        };
        app.handle_export(&PresetsAction::Export, &mut state);
        assert_eq!(state.finished, Some(true));

        let exported = |file: &str| {
            InputPreset::load(&dir.path().join("export").join(file)).expect("a voice preset")
        };
        assert_eq!(exported("Mine.toml").eq.gains_db[0], 6.0, "as saved");
        assert_eq!(exported("Loud.toml").makeup_db, 9.0);
        assert!(!dir.path().join("export/Mine.fac").exists());

        // A second export asks about exactly the voice files that are there.
        let mut again = ExportState {
            presets: state.presets.clone(),
            selected: state.selected.clone(),
            ..ExportState::default()
        };
        app.handle_export(&PresetsAction::Export, &mut again);
        assert_eq!(again.collisions, ["Loud", "Mine"]);
    }

    #[test]
    fn a_quit_files_each_lanes_edits_in_its_own_store_and_a_restart_brings_both_back() {
        let (mut app, _engine, dir) = started_with(saved_settings(OUT));
        app.handle(&[UiAction::SetEffect(Effect::Bass, 7.0)]);
        app.handle(&[UiAction::SetEditDirection(IN)]);
        app.handle(&[UiAction::SetBandGain(0, 6.0), UiAction::SetMasterGain(2.0)]);
        app.handle(&[UiAction::SetEditDirection(OUT)]);

        app.shutdown();

        assert_eq!(music_autosaves(&dir), ["Beta.fac"]);
        assert_eq!(voice_files(&dir, "AutoSave"), ["Loud.toml"]);
        let stashed = saved_voice(&dir, "AutoSave/Loud.toml");
        assert_eq!(stashed.eq.gains_db[0], 6.0);
        assert_eq!(stashed.makeup_db, 2.0);
        assert_eq!(stashed.chain, "podcast", "the rest is Loud's");

        for edit in [OUT, IN] {
            let next = started_again(&dir, edit);
            assert_eq!(
                next.lane_preset(OUT),
                Some(("Beta", true)),
                "editing {edit:?}"
            );
            assert_eq!(
                next.lane_preset(IN),
                Some(("Loud", true)),
                "editing {edit:?}"
            );
            assert_eq!(next.input_params().makeup_db, 2.0);
            assert_eq!(next.input_params().bands().1[0], 6.0);
            assert!((next.params().effect(Effect::Bass) - 0.7).abs() < 0.01);
        }
    }

    #[test]
    fn a_microphone_brings_back_the_voice_preset_it_was_last_used_with() {
        const HEADSET: &str = "alsa_input.headset";
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        let mut devices = two_lane_devices();
        devices.push(device(HEADSET, IN, false));
        engine.feed(AudioToUi::Devices(devices));
        app.poll_audio();
        let index = |app: &App, name: &str| {
            app.state
                .devices
                .iter()
                .position(|d| d.name == name)
                .expect("listed")
        };

        app.handle(&[UiAction::SelectInput(index(&app, MIC))]);
        app.handle(&[UiAction::SelectPreset(at(&app, "Quiet"))]);
        // The first time, the headset leaves the preset alone; then it is given its own.
        app.handle(&[UiAction::SelectInput(index(&app, HEADSET))]);
        assert_eq!(app.lane_preset(IN), Some(("Quiet", false)));
        app.handle(&[UiAction::SelectPreset(at(&app, "Loud"))]);

        app.handle(&[UiAction::SelectInput(index(&app, MIC))]);
        assert_eq!(app.lane_preset(IN), Some(("Quiet", false)));
        app.handle(&[UiAction::SelectInput(index(&app, HEADSET))]);
        assert_eq!(app.lane_preset(IN), Some(("Loud", false)));

        assert_eq!(app.settings.preset_for_device(MIC, IN), Some("Quiet"));
        assert_eq!(app.settings.preset_for_device(HEADSET, IN), Some("Loud"));
        assert_eq!(app.settings.preset_for_device(MIC, OUT), None);
        assert_eq!(
            app.lane_preset(OUT),
            Some(("Beta", false)),
            "the speakers kept theirs"
        );
    }

    #[test]
    fn the_makeup_is_part_of_a_voice_preset_and_the_speakers_master_gain_is_not_part_of_a_fac() {
        let (mut app, _engine, _dir) = editing_the_microphone();
        app.handle(&[UiAction::SetMasterGain(3.0)]);
        assert_eq!(app.lane_preset(IN), Some(("Loud", true)));

        app.handle(&[
            UiAction::SetEditDirection(OUT),
            UiAction::SetMasterGain(1.0),
        ]);
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert_eq!(app.settings.master_gain, 1.0, "a setting over every .fac");
    }

    #[test]
    fn a_pinned_noise_suppression_level_is_never_written_into_a_saved_voice_preset() {
        let (mut app, engine, dir) = editing_the_microphone();
        app.set_noise_suppression(NoiseSuppressionOverride::Strong);
        assert!(engine.input_params().expect("published").rnnoise);
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        app.handle(&[UiAction::SavePresetAs("Mine".into())]);

        let saved = saved_voice(&dir, "Mine.toml");
        assert!(saved.denoise.is_none(), "Loud has no denoiser of its own");
        assert!(!saved.rnnoise);
    }

    #[test]
    fn the_output_device_preference_lists_the_speakers_devices_with_the_speakers_presets() {
        // A microphone remembers its voice preset in the same list, but it is neither an output
        // device nor something a `.fac` can be given.
        let (mut app, _engine, _dir) = started_with(saved_settings(IN));
        let remember = |app: &mut App, node: &str, preset: &str, direction| {
            app.settings
                .remember_device_preset(node, node, preset, "", direction);
        };
        remember(&mut app, MIC, "Quiet", IN);
        remember(&mut app, SPEAKERS, "Alpha", OUT);
        remember(&mut app, HEADPHONES, "Beta", OUT);

        let mut pane = app.settings_state();
        let ids = |pane: &SettingsState| -> Vec<String> {
            pane.devices.iter().map(|d| d.id.clone()).collect()
        };
        assert_eq!(ids(&pane), [SPEAKERS, HEADPHONES]);
        assert_eq!(
            pane.presets,
            ["Alpha", "Beta"],
            "the .fac list, on the microphone too"
        );
        assert_eq!(pane.devices[1].preset, Some(1));

        app.handle_settings(
            &SettingsAction::SetDevicePreset {
                device: 0,
                preset: 1,
            },
            &mut pane,
        );
        assert_eq!(app.settings.preset_for_device(SPEAKERS, OUT), Some("Beta"));
        assert_eq!(app.settings.preset_for_device(MIC, IN), Some("Quiet"));
        assert_eq!(pane.devices[0].preset, Some(1), "the row follows");

        app.handle_settings(&SettingsAction::MoveDeviceUp(0), &mut pane);
        assert_eq!(
            ids(&pane),
            [SPEAKERS, HEADPHONES],
            "nothing above the first row"
        );
        app.handle_settings(&SettingsAction::MoveDeviceDown(0), &mut pane);
        assert_eq!(ids(&pane), [HEADPHONES, SPEAKERS]);
        let order: Vec<&str> = app
            .settings
            .device_configs
            .iter()
            .map(|c| c.device_id.as_str())
            .collect();
        assert_eq!(
            order,
            [MIC, HEADPHONES, SPEAKERS],
            "the microphone stays put"
        );
        assert_eq!(pane.settings.device_configs, app.settings.device_configs);

        app.handle_settings(&SettingsAction::RemoveDevice(1), &mut pane);
        assert_eq!(ids(&pane), [HEADPHONES]);
        assert_eq!(app.settings.preset_for_device(SPEAKERS, OUT), None);
        assert_eq!(app.settings.preset_for_device(MIC, IN), Some("Quiet"));
    }

    #[test]
    fn reset_presets_drops_both_lanes_edits_and_the_engine_runs_what_was_saved() {
        let (mut app, engine, dir) = started_with(saved_settings(OUT));
        assert!(
            !app.settings_state().can_reset_presets,
            "nothing to lose yet"
        );
        app.handle(&[UiAction::SetEffect(Effect::Bass, 9.0)]);
        app.handle(&[UiAction::SetEditDirection(IN)]);
        app.handle(&[UiAction::SetBandGain(0, 6.0)]);
        app.handle(&[UiAction::SelectPreset(at(&app, "Quiet"))]);
        app.handle(&[UiAction::SetMasterGain(5.0)]);
        app.handle(&[UiAction::SetEditDirection(OUT)]);
        assert_eq!(voice_files(&dir, "AutoSave"), ["Loud.toml"]);
        assert_eq!(engine.input_params().expect("published").makeup_db, 5.0);

        let mut pane = app.settings_state();
        assert!(pane.can_reset_presets);
        app.handle_settings(&SettingsAction::ResetPresets, &mut pane);

        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert_eq!(app.lane_preset(IN), Some(("Quiet", false)));
        assert_eq!(music_autosaves(&dir), Vec::<String>::new());
        assert_eq!(voice_files(&dir, "AutoSave"), Vec::<String>::new());
        assert!((engine.params().expect("published").effect(Effect::Bass) - 0.6).abs() < 0.02);
        assert_eq!(
            engine.input_params().expect("published").makeup_db,
            0.0,
            "the lane off screen runs Quiet as saved at once"
        );
        assert_eq!(app.state.direction, OUT, "the window stayed where it was");
        assert_eq!(app.settings.device_direction, OUT);
        assert_eq!(
            app.state.notification.as_deref(),
            Some("Presets are restored to factory defaults")
        );
        assert!(!app.settings_state().can_reset_presets);

        app.handle(&[UiAction::SetEditDirection(IN)]);
        assert!(!entry(&app, "Loud").modified);
        assert_eq!(app.state.master_gain_db, 0.0);
    }

    // ---- the calibration wizard (0.4.0 design §8), through a fake feed -------------------------

    use crate::calibration::{CLEAN_VOICE, Failure};
    use fxsound_ui::dialogs::{CalibrationAction, CalibrationPhase};

    const FIFINE: &str = "fifine Microphone Analogue Stereo";
    const CALIBRATED: &str = "Calibrated — fifine Microphone Analogue Stereo";

    /// A voice store in `dir` holding the voice presets the package ships, as the real one does.
    fn shipped_voices(dir: &tempfile::TempDir) -> InputPresetStore {
        let shipped = InputPreset::load_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/presets/Input"),
        )
        .expect("the shipped voice presets");
        voice_store_for_tests(
            &shipped,
            &dir.path().join("voice-factory"),
            dir.path().join("user").join("Input"),
        )
    }

    /// A start on the shipped voice presets with the window on `edit`, the speakers on the
    /// headphones, and the microphone lane attached to the fifine and processing — `processing`
    /// says whether it is. Everything the start-up said is already taken.
    fn calibrating_with(
        edit: DeviceDirection,
        processing: bool,
    ) -> (App, FakeEngine, tempfile::TempDir, Instant) {
        let engine = FakeEngine::new();
        let (store, dir) = music_store();
        let mut settings = saved_settings(edit);
        settings.input_preset = CLEAN_VOICE.to_owned();
        let mut app = App::start_for_tests(settings, store, shipped_voices(&dir), &engine);
        engine.feed(AudioToUi::Devices(vec![
            device(SPEAKERS, OUT, true),
            device(HEADPHONES, OUT, false),
            AudioDevice {
                description: FIFINE.to_owned(),
                form_factor: "microphone".to_owned(),
                ..device(MIC, IN, true)
            },
        ]));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        engine.feed(attached(IN, Some(MIC)));
        engine.feed(AudioToUi::Status {
            direction: IN,
            status: status(processing),
        });
        let t0 = Instant::now();
        app.poll_audio_at(t0);
        let _ = engine.take_sent();
        let _ = engine.take_events();
        let _ = app.drain_events();
        (app, engine, dir, t0)
    }

    fn calibrating(edit: DeviceDirection) -> (App, FakeEngine, tempfile::TempDir, Instant) {
        calibrating_with(edit, true)
    }

    /// The input lane's accumulators after `seconds` at `rms_db` RMS with peaks at `peak_db`, at
    /// 48 kHz, and the floor estimator at `floor_db`.
    fn capture(seconds: f32, rms_db: f32, peak_db: f32, floor_db: f32) -> Meters {
        let frames = (seconds * 48_000.0) as u64;
        Meters {
            capture_frames: frames,
            capture_sum_squares: 10_f64.powf(f64::from(rms_db) / 10.0) * frames as f64,
            capture_peak: 10_f32.powf(peak_db / 20.0),
            noise_floor_db: floor_db,
            input_rms_db: rms_db,
            ..Meters::default()
        }
    }

    fn later(t0: Instant, seconds: f32) -> Instant {
        t0 + std::time::Duration::from_secs_f32(seconds)
    }

    /// The meter feed of one run, a reading per phase with the time it is handed over: a −55 dBFS
    /// room (the estimator at −57), speech at −28 RMS peaking at −8, loud speech that never clips.
    fn script() -> [(f32, Meters); 4] {
        [
            (3.0, capture(3.0, -55.0, -40.0, -57.0)),
            (8.0, capture(5.0, -28.0, -8.0, -56.0)),
            (10.0, capture(2.0, -16.0, -2.0, -56.0)),
            (10.5, capture(2.0, -16.0, -2.0, -56.0)),
        ]
    }

    /// A second sitting that measures differently at every number the recommendation turns on,
    /// and still lands on Clean Voice: a −50 dBFS room (the estimator at −52), speech at −22 RMS
    /// peaking at −2. The gate comes to −44, the compressor to −28 and the makeup to +8, where
    /// [`script`] gives −49, −34 and +14.
    fn nearer_script() -> [(f32, Meters); 4] {
        [
            (3.0, capture(3.0, -50.0, -38.0, -52.0)),
            (8.0, capture(5.0, -22.0, -2.0, -51.0)),
            (10.0, capture(2.0, -12.0, -1.0, -51.0)),
            (10.5, capture(2.0, -12.0, -1.0, -51.0)),
        ]
    }

    /// Press Start at `t0` and feed the script's first `steps` readings.
    fn run_wizard(app: &mut App, engine: &FakeEngine, t0: Instant, steps: usize) {
        run_wizard_on(app, engine, t0, script(), steps);
    }

    /// Press Start at `t0` and feed the first `steps` readings of `feed`.
    fn run_wizard_on(
        app: &mut App,
        engine: &FakeEngine,
        t0: Instant,
        feed: [(f32, Meters); 4],
        steps: usize,
    ) {
        app.calibration_action_at(CalibrationAction::Start, t0);
        // The lane is processing already: the silence starts at the next look.
        app.poll_audio_at(t0);
        for (seconds, meters) in feed.into_iter().take(steps) {
            engine.set_meters(IN, meters);
            app.poll_audio_at(later(t0, seconds));
        }
    }

    /// The numbers of the input chain that a calibration sets: high-pass, gate, compressor
    /// threshold and ratio, makeup and ceiling.
    fn calibrated_numbers(params: &InputDspParams) -> (f32, f32, f32, f32, f32, f32) {
        (
            params.highpass_hz,
            params.gate_threshold_db,
            params.compressor_threshold_db,
            params.compressor_ratio,
            params.makeup_db,
            params.ceiling_db,
        )
    }

    /// The `KeepInputAwake` requests among `sent`, in order.
    fn awake_requests(sent: &[UiToAudio]) -> Vec<bool> {
        sent.iter()
            .filter_map(|message| match message {
                UiToAudio::KeepInputAwake(awake) => Some(*awake),
                _ => None,
            })
            .collect()
    }

    fn phase(app: &App, now: Instant) -> Option<CalibrationPhase> {
        app.calibration_view_at(now).map(|view| view.phase)
    }

    #[test]
    fn a_run_holds_the_microphone_resets_the_input_lane_on_each_phase_and_lets_go_after_the_last() {
        let (mut app, engine, _dir, t0) = calibrating(OUT);
        assert!(app.open_calibration());
        let intro = app.calibration_view_at(t0).expect("open");
        assert_eq!(intro.phase, CalibrationPhase::Intro);
        assert_eq!(intro.device, FIFINE);
        assert!(
            intro.can_start(),
            "a microphone is attached, so Start is live"
        );
        assert!(
            engine.take_sent().is_empty(),
            "opening asks nothing of the engine"
        );

        app.calibration_action_at(CalibrationAction::Start, t0);
        assert_eq!(engine.take_sent(), [UiToAudio::KeepInputAwake(true)]);
        assert!(app.calibration_is_live());

        let mut steps = script().into_iter();
        app.poll_audio_at(t0);
        assert_eq!(phase(&app, t0), Some(CalibrationPhase::Silence));
        for expected in [
            CalibrationPhase::Speech,
            CalibrationPhase::Loud,
            CalibrationPhase::Analysing,
            CalibrationPhase::Result,
        ] {
            let (seconds, meters) = steps.next().expect("a step");
            engine.set_meters(IN, meters);
            app.poll_audio_at(later(t0, seconds));
            assert_eq!(phase(&app, later(t0, seconds)), Some(expected));
        }
        assert_eq!(
            engine.take_events(),
            [(IN, DspEvent::ResetCaptureStats); 3],
            "one reset on entry to each timed phase, all on the microphone's lane"
        );
        assert_eq!(awake_requests(&engine.take_sent()), [false]);
        assert!(!app.calibration_is_live());

        let view = app.calibration_view_at(later(t0, 11.0)).expect("open");
        let result = view.result.as_ref().expect("the result");
        assert_eq!(result.preset, CLEAN_VOICE);
        assert!((result.floor_db + 57.0).abs() < 0.01, "{result:?}");
        assert!((result.speech_rms_db + 28.0).abs() < 0.01, "{result:?}");
        assert_eq!(
            result.lines,
            [
                "High-pass 80 Hz",
                "Gate −49 dB",
                "Compressor −34 dB, ratio 3:1",
                "Makeup gain +14 dB",
                "Ceiling −3 dB",
                "Noise suppression: Mild",
            ]
        );
        assert!(view.can_apply());
        assert!(
            app.settings().calibration.is_none(),
            "nothing is kept until Apply"
        );
    }

    #[test]
    fn apply_writes_the_calibrated_voice_preset_and_selects_it_on_the_microphone_lane() {
        let (mut app, engine, dir, t0) = calibrating(OUT);
        app.open_calibration();
        run_wizard(&mut app, &engine, t0, 4);
        let _ = engine.take_sent();
        let _ = app.drain_events();

        app.calibration_action_at(CalibrationAction::Apply, later(t0, 12.0));
        assert!(!app.calibration_open(), "Apply closes the wizard");
        assert_eq!(voice_files(&dir, ""), [format!("{CALIBRATED}.toml")]);

        // Selected on the microphone, with the window left on the speakers and their preset.
        assert_eq!(app.lane_preset(IN), Some((CALIBRATED, false)));
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert_eq!(app.state.direction, OUT);
        assert_eq!(app.settings.input_preset, CALIBRATED);

        // The microphone chain runs it now.
        let params = engine.input_params().expect("published");
        assert_eq!(
            calibrated_numbers(&params),
            (80.0, -49.0, -34.0, 3.0, 14.0, -3.0)
        );
        assert_eq!(params.denoise_level, DenoiseLevel::Light);
        assert!(params.gate_on && params.compressor_on);

        // The file is Clean Voice with the numbers written over it.
        let written = app.voice_presets.load_saved(CALIBRATED).expect("on disk");
        let clean = app.voice_presets.load_saved(CLEAN_VOICE).expect("shipped");
        assert_eq!(written.eq, clean.eq);
        assert_eq!(written.deesser, clean.deesser);
        assert!(
            written.description.contains(FIFINE),
            "{}",
            written.description
        );

        // The record, the microphone's memory, the stream and the notice.
        let record = app.settings().calibration.clone().expect("recorded");
        assert_eq!(
            (record.preset.as_str(), record.device.as_str()),
            (CALIBRATED, MIC)
        );
        assert!((record.noise_floor_db + 57.0).abs() < 0.01, "{record:?}");
        assert!((record.speech_peak_db + 8.0).abs() < 0.01, "{record:?}");
        assert_eq!(record.clipped_ratio, 0.0);
        assert!(record.unix_time > 1_700_000_000);
        assert!(
            app.settings()
                .device_configs
                .iter()
                .any(|config| config.device_id == MIC
                    && config.direction == IN
                    && config.preset == CALIBRATED),
            "the fifine brings its calibrated preset back"
        );
        let events = app.drain_events();
        assert!(
            events.contains(&AppEvent::PresetChanged {
                direction: IN,
                name: Some(CALIBRATED.to_owned()),
                modified: false,
            }),
            "{events:?}"
        );
        assert!(events.contains(&AppEvent::Calibrated(record)), "{events:?}");
        assert!(app.unsaid_changes().is_empty());
        assert_eq!(
            app.state.notification.as_deref(),
            Some(format!("New preset {CALIBRATED} is saved.").as_str())
        );
        assert!(
            awake_requests(&engine.take_sent()).is_empty(),
            "the microphone was already let go"
        );
    }

    #[test]
    fn calibrating_the_same_microphone_again_overwrites_its_preset_and_keeps_a_backup() {
        let (mut app, engine, dir, t0) = calibrating(IN);
        app.open_calibration();
        run_wizard(&mut app, &engine, t0, 4);
        app.calibration_action_at(CalibrationAction::Apply, later(t0, 12.0));
        assert_eq!(app.state.master_gain_db, 14.0, "the first sitting's makeup");

        // Nearer the microphone in a noisier room: every number the wizard sets moves.
        let t1 = later(t0, 20.0);
        assert!(app.open_calibration());
        run_wizard_on(&mut app, &engine, t1, nearer_script(), 4);
        let view = app.calibration_view_at(later(t1, 11.0)).expect("open");
        assert_eq!(
            view.result.as_ref().map(|result| result.preset.as_str()),
            Some(CLEAN_VOICE),
            "the same shipped preset under it, so only the measured numbers differ"
        );
        app.calibration_action_at(CalibrationAction::Apply, later(t1, 12.0));

        // One preset, listed once, still selected, and said to be overwritten.
        assert_eq!(voice_files(&dir, ""), [format!("{CALIBRATED}.toml")]);
        assert_eq!(
            app.state
                .presets
                .iter()
                .filter(|p| p.name == CALIBRATED)
                .count(),
            1
        );
        assert_eq!(
            app.state.preset().map(|p| p.name.as_str()),
            Some(CALIBRATED)
        );
        assert_eq!(
            app.state.notification.as_deref(),
            Some(format!("Changes to preset {CALIBRATED} are saved.").as_str())
        );

        // The microphone chain runs the second sitting's numbers, and the window, which is on
        // the microphone, shows its makeup as the output gain.
        let params = engine.input_params().expect("published");
        assert_eq!(
            calibrated_numbers(&params),
            (80.0, -44.0, -28.0, 3.0, 8.0, -3.0)
        );
        assert_eq!(params.denoise_level, DenoiseLevel::Light);
        assert_eq!(app.state.master_gain_db, 8.0);
        // Gate, compressor threshold and makeup as a preset file has them.
        let on_disk = |preset: &InputPreset| {
            (
                preset.gate.as_ref().map(|gate| gate.threshold_db),
                preset.compressor.as_ref().map(|comp| comp.threshold_db),
                preset.makeup_db,
            )
        };
        let written = saved_voice(&dir, &format!("{CALIBRATED}.toml"));
        assert_eq!(on_disk(&written), (Some(-44.0), Some(-28.0), 8.0));
        let record = app.settings().calibration.clone().expect("recorded");
        assert!((record.noise_floor_db + 52.0).abs() < 0.01, "{record:?}");
        assert!((record.speech_rms_db + 22.0).abs() < 0.01, "{record:?}");

        // The first sitting's preset is kept beside it, as any overwrite of a user preset is
        // (design §8).
        let backup = saved_voice(&dir, &format!("{CALIBRATED}.toml.bak"));
        assert_eq!(backup.name, CALIBRATED);
        assert_eq!(on_disk(&backup), (Some(-49.0), Some(-34.0), 14.0));
    }

    #[test]
    fn apply_keeps_unsaved_edits_to_the_voice_preset_it_replaces() {
        let (mut app, engine, dir, t0) = calibrating(IN);
        app.handle(&[UiAction::SetMasterGain(3.0)]);
        assert!(app.state.preset().is_some_and(|p| p.modified));
        app.open_calibration();
        run_wizard(&mut app, &engine, t0, 4);
        app.calibration_action_at(CalibrationAction::Apply, later(t0, 12.0));
        assert_eq!(
            voice_files(&dir, "AutoSave"),
            [format!("{CLEAN_VOICE}.toml")],
            "stashed on the way out, as any preset switch does"
        );
    }

    #[test]
    fn cancel_or_close_at_every_phase_leaves_the_microphone_released() {
        for steps in 0..=5 {
            let (mut app, engine, _dir, t0) = calibrating(OUT);
            app.open_calibration();
            if steps > 0 {
                run_wizard(&mut app, &engine, t0, steps - 1);
            }
            let shown = phase(&app, later(t0, 11.0)).expect("open");
            let action = if shown == CalibrationPhase::Result {
                CalibrationAction::Close
            } else {
                CalibrationAction::Cancel
            };
            app.calibration_action_at(action, later(t0, 11.0));
            assert!(!app.calibration_open(), "{shown:?}");
            let requests = awake_requests(&engine.take_sent());
            let held = requests.iter().filter(|awake| **awake).count();
            let released = requests.iter().filter(|awake| !**awake).count();
            assert_eq!(held, usize::from(steps > 0), "{shown:?}: {requests:?}");
            assert_eq!(held, released, "{shown:?}: {requests:?}");
            assert_eq!(
                requests.last(),
                (steps > 0).then_some(&false).as_ref().copied()
            );
            assert!(app.settings().calibration.is_none());
        }
    }

    #[test]
    fn a_microphone_that_goes_in_the_middle_of_a_run_fails_it_and_lets_it_go() {
        let (mut app, engine, _dir, t0) = calibrating(OUT);
        app.open_calibration();
        run_wizard(&mut app, &engine, t0, 1);
        assert_eq!(phase(&app, later(t0, 3.0)), Some(CalibrationPhase::Speech));
        engine.feed(attached(IN, None));
        app.poll_audio_at(later(t0, 4.0));

        let view = app.calibration_view_at(later(t0, 4.0)).expect("open");
        assert_eq!(view.phase, CalibrationPhase::Failed);
        assert_eq!(view.failure, Failure::MicrophoneChanged.text());
        assert!(!view.can_start(), "no microphone to retry on");
        assert_eq!(awake_requests(&engine.take_sent()), [true, false]);

        // It comes back: Retry measures it again.
        engine.feed(attached(IN, Some(MIC)));
        app.poll_audio_at(later(t0, 5.0));
        assert!(
            app.calibration_view_at(later(t0, 5.0))
                .expect("open")
                .can_start()
        );
        app.calibration_action_at(CalibrationAction::Retry, later(t0, 5.0));
        assert_eq!(awake_requests(&engine.take_sent()), [true]);
    }

    #[test]
    fn a_lane_that_never_starts_fails_after_five_seconds_and_lets_the_microphone_go() {
        let (mut app, engine, _dir, t0) = calibrating_with(OUT, false);
        app.open_calibration();
        app.calibration_action_at(CalibrationAction::Start, t0);
        app.poll_audio_at(later(t0, 4.9));
        assert_eq!(
            phase(&app, later(t0, 4.9)),
            Some(CalibrationPhase::Silence),
            "the silence page, waiting for the lane"
        );
        assert!(engine.take_events().is_empty(), "nothing is measured yet");
        app.poll_audio_at(later(t0, 5.0));
        let view = app.calibration_view_at(later(t0, 5.0)).expect("open");
        assert_eq!(view.phase, CalibrationPhase::Failed);
        assert_eq!(
            view.failure,
            "The microphone sent no sound within 5 seconds."
        );
        assert_eq!(awake_requests(&engine.take_sent()), [true, false]);
    }

    #[test]
    fn the_window_going_or_the_application_quitting_mid_run_lets_the_microphone_go() {
        let (mut app, engine, _dir, t0) = calibrating(OUT);
        app.open_calibration();
        run_wizard(&mut app, &engine, t0, 1);
        app.cancel_calibration();
        assert_eq!(awake_requests(&engine.take_sent()), [true, false]);
        assert!(!app.calibration_open());

        let (mut app, engine, _dir, t0) = calibrating(OUT);
        app.open_calibration();
        run_wizard(&mut app, &engine, t0, 2);
        app.shutdown();
        assert_eq!(awake_requests(&engine.take_sent()), [true, false]);
        assert!(engine.is_shut_down());
    }

    #[test]
    fn without_a_microphone_on_the_input_lane_the_wizard_does_not_open() {
        let (mut app, engine, _dir, _t0) = calibrating(OUT);
        engine.feed(attached(IN, None));
        app.poll_audio();
        assert!(!app.open_calibration());
        assert_eq!(app.calibration_view(), None);
        app.handle_calibration(CalibrationAction::Start);
        assert!(engine.take_sent().is_empty());
    }

    #[test]
    fn the_pane_shows_a_calibration_applied_while_it_is_open() {
        let (mut app, engine, _dir, t0) = calibrating(OUT);
        let mut pane = app.settings_state();
        assert_eq!(pane.calibration_text(), "Not calibrated yet");
        app.open_calibration();
        run_wizard(&mut app, &engine, t0, 4);
        app.calibration_action_at(CalibrationAction::Apply, later(t0, 12.0));
        app.refresh_settings_state(&mut pane);
        assert_eq!(pane.settings.calibration, app.settings().calibration);
        assert!(
            pane.calibration_text().starts_with("Floor −57 dB"),
            "{}",
            pane.calibration_text()
        );
    }

    #[test]
    fn at_the_user_preset_cap_apply_is_refused_and_the_result_stays_up() {
        let (mut app, engine, dir, t0) = calibrating(OUT);
        app.settings.max_user_presets = 10;
        for n in 0..10 {
            app.voice_presets
                .save_as(&InputPreset::default(), &format!("Mine {n}"))
                .expect("saved");
        }
        app.open_calibration();
        run_wizard(&mut app, &engine, t0, 4);
        app.calibration_action_at(CalibrationAction::Apply, later(t0, 12.0));
        assert_eq!(
            phase(&app, later(t0, 12.0)),
            Some(CalibrationPhase::Result),
            "Close is still the user's to press"
        );
        assert_eq!(
            app.state.notification.as_deref(),
            Some("Reached the limit on new presets.")
        );
        assert!(!voice_files(&dir, "").contains(&format!("{CALIBRATED}.toml")));
        assert!(app.settings().calibration.is_none());
    }

    // ---- the upstream review's controller items: U2, U6, U15, U16 -----------------------------

    use crate::cli::PresetCommand as P;

    fn preset_with(name: &str, bands: &[(f32, f32)]) -> Preset {
        Preset {
            name: name.to_owned(),
            eq_bands: bands.iter().map(|&(hz, db)| EqBand::new(hz, db)).collect(),
            ..Preset::default()
        }
    }

    /// A start on `num_bands` bands against a fake engine, with `presets` as the speakers' factory
    /// set and the first of them the output lane's saved preset.
    fn started_on(num_bands: u32, presets: &[Preset]) -> (App, FakeEngine, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("scratch directory");
        let factory = dir.path().join("factory");
        std::fs::create_dir_all(&factory).expect("factory directory");
        for preset in presets {
            let file = factory.join(format!("{}.fac", preset.name));
            fxsound_preset::save(preset, &file).expect("write");
        }
        let mut music = PresetStore::with_dirs(vec![factory], dir.path().join("user"));
        music.rescan();
        let mut settings = Settings::default();
        settings.num_bands = num_bands;
        presets[0].name.clone_into(&mut settings.output_preset);
        let engine = FakeEngine::new();
        let app = App::start_for_tests(settings, music, voices(&dir), &engine);
        (app, engine, dir)
    }

    fn pick(app: &mut App, name: &str) {
        let at = app
            .state
            .presets
            .iter()
            .position(|p| p.name == name)
            .unwrap_or_else(|| panic!("no preset {name}"));
        app.handle(&[UiAction::SelectPreset(at)]);
    }

    #[test]
    fn a_preset_of_another_band_count_lands_on_the_users_band_count() {
        // Upstream `DfxDspEq.cpp:127-247`: a user on thirty-one bands who picks a ten-band preset
        // stays on thirty-one bands, the preset's curve fitted onto them by position (U2).
        let ten: Vec<(f32, f32)> = fxsound_core::eq::DEFAULT_CENTERS_HZ
            .iter()
            .zip([6.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 0.0, -3.0])
            .map(|(&hz, db)| (hz, db))
            .collect();
        let (app, engine, _dir) = started_on(31, &[preset_with("Ten", &ten)]);

        let preset_centres: Vec<f32> = ten.iter().map(|b| b.0).collect();
        let preset_gains: Vec<f32> = ten.iter().map(|b| b.1).collect();
        assert_eq!(app.state.eq_bands.len(), 31);
        assert_eq!(
            centres(&app),
            ladder(31),
            "the live ladder, not the preset's"
        );
        assert_eq!(
            gains(&app),
            fxsound_dsp::eq::fit_preset_gains(&preset_centres, &preset_gains, &ladder(31))
        );
        assert_eq!(gains(&app)[0], 6.0);
        assert_eq!(gains(&app)[30], -3.0);
        assert_eq!(
            app.settings.num_bands, 31,
            "the preset does not move the setting"
        );
        assert_eq!(engine.params().expect("published").num_bands, 31);
        assert_eq!(
            app.lane_preset(OUT),
            Some(("Ten", false)),
            "loading is no edit"
        );
    }

    #[test]
    fn a_preset_of_the_users_band_count_brings_its_own_frequencies_and_another_keeps_them() {
        // Equal counts copy the centres too (`DfxDspEq.cpp:229-241`); a different count keeps the
        // live ladder, whatever moved it.
        let moved: Vec<(f32, f32)> = (0..10)
            .map(|band| (40.0 * 2f32.powi(band), if band == 3 { 5.0 } else { 0.0 }))
            .collect();
        let five = [
            (62.5, 3.0),
            (250.0, 0.0),
            (1000.0, 0.0),
            (4000.0, 0.0),
            (16000.0, -2.0),
        ];
        let (mut app, _engine, _dir) = started_on(
            10,
            &[preset_with("Moved", &moved), preset_with("Five", &five)],
        );
        let moved_centres: Vec<f32> = moved.iter().map(|b| b.0).collect();
        assert_eq!(centres(&app), moved_centres);
        assert_eq!(gains(&app)[3], 5.0);

        pick(&mut app, "Five");
        assert_eq!(centres(&app), moved_centres, "the ladder the user is on");
        assert_eq!(
            gains(&app),
            fxsound_dsp::eq::remap_band_gains(&[3.0, 0.0, 0.0, 0.0, -2.0], 10)
        );
        assert_eq!(app.params().num_bands, 10);
    }

    #[test]
    fn a_preset_with_no_equalizer_turns_it_on_and_flat_on_the_users_ladder() {
        // The original's "old preset" (`DfxDspEq.cpp:144-158`).
        let (mut app, _engine, _dir) = started_on(15, &[preset_with("Alpha", &[])]);
        app.handle(&[UiAction::SetEqEnabled(false), UiAction::SetBandGain(2, 7.0)]);
        let mut old = preset_with("Old", &[]);
        old.eq_on = false;
        app.apply_preset(&old);
        assert!(app.state.eq_on);
        assert!(app.params().eq_on);
        assert_eq!(app.state.eq_bands.len(), 15);
        assert!(gains(&app).iter().all(|&g| g == 0.0), "{:?}", gains(&app));
        assert_eq!(centres(&app), ladder(15));
    }

    #[test]
    fn a_start_with_no_preset_to_load_still_shows_the_saved_band_count() {
        let engine = FakeEngine::new();
        let dir = tempfile::tempdir().expect("scratch directory");
        let music = PresetStore::with_dirs(Vec::new(), dir.path().join("user"));
        let mut settings = Settings::default();
        settings.num_bands = 20;
        let app = App::start_for_tests(settings, music, voices(&dir), &engine);
        assert_eq!(app.state.eq_bands.len(), 20);
        assert_eq!(centres(&app), ladder(20));
        assert_eq!(engine.params().expect("published").num_bands, 20);
    }

    /// The speakers playing `Alpha` and the headphones last used with `Beta`, both remembered in
    /// `device_configs` the way a pick remembers them.
    fn speakers_on_alpha(tag: &str) -> App {
        let mut app = app_with_two_presets(tag);
        app.handle(&[UiAction::SelectOutput(0), UiAction::SelectPreset(1)]);
        app.handle(&[UiAction::SelectOutput(1), UiAction::SelectPreset(0)]);
        assert_eq!(selected(&app, OUT), Some("alsa_output.speakers"));
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
        assert_eq!(
            app.settings
                .preset_for_device("alsa_output.headphones", OUT),
            Some("Beta")
        );
        app
    }

    fn row_of(pane: &SettingsState, node_name: &str) -> usize {
        pane.devices
            .iter()
            .position(|row| row.id == node_name)
            .unwrap_or_else(|| panic!("no row for {node_name}"))
    }

    #[test]
    fn a_device_preset_set_in_settings_for_the_device_playing_applies_at_once() {
        // Upstream 83ccf5e (`FxOutputPreference.cpp:57-61`); the row of a device that is not
        // playing only remembers.
        let mut app = speakers_on_alpha("settings-row");
        let mut pane = app.settings_state();
        let alpha = app.presets.index_of("Alpha").expect("Alpha");
        let beta = app.presets.index_of("Beta").expect("Beta");

        let headphones = row_of(&pane, "alsa_output.headphones");
        app.handle_settings(
            &SettingsAction::SetDevicePreset {
                device: headphones,
                preset: alpha,
            },
            &mut pane,
        );
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
        assert_eq!(
            app.settings
                .preset_for_device("alsa_output.headphones", OUT),
            Some("Alpha")
        );

        let speakers = row_of(&pane, "alsa_output.speakers");
        app.handle_settings(
            &SettingsAction::SetDevicePreset {
                device: speakers,
                preset: beta,
            },
            &mut pane,
        );
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert_eq!(app.settings.output_preset, "Beta");
        assert_eq!(
            pane.devices[row_of(&pane, "alsa_output.speakers")].preset,
            Some(beta)
        );
        assert_eq!(pane.settings.device_configs, app.settings.device_configs);
    }

    #[test]
    fn a_device_preset_set_in_settings_while_the_microphone_is_edited_reaches_the_speakers() {
        let mut app = speakers_on_alpha("settings-row-off-screen");
        let _voices = with_voice_presets(&mut app);
        app.handle(&[UiAction::SetEditDirection(IN)]);
        let mut pane = app.settings_state();
        let beta = app.presets.index_of("Beta").expect("Beta");
        app.handle_settings(
            &SettingsAction::SetDevicePreset {
                device: row_of(&pane, "alsa_output.speakers"),
                preset: beta,
            },
            &mut pane,
        );
        assert_eq!(app.state.direction, IN, "the window stays where it was");
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert_eq!(app.settings.output_preset, "Beta");
    }

    #[test]
    fn deleting_a_preset_selects_the_one_the_device_remembers() {
        // Upstream 7f160b6 (`FxController::deletePreset`): the device's preset, not the
        // neighbour, which here would be Beta.
        let mut app = speakers_on_alpha("delete-to-device");
        app.presets
            .save_as(&Preset::default(), "Mine")
            .expect("saved");
        app.refresh_preset_list();
        pick(&mut app, "Mine");
        app.settings.remember_device_preset(
            "alsa_output.speakers",
            "alsa_output.speakers",
            "Alpha",
            "speaker",
            OUT,
        );
        app.handle(&[UiAction::DeletePreset]);
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
    }

    #[test]
    fn deleting_the_devices_own_preset_falls_back_to_the_neighbour() {
        let mut app = speakers_on_alpha("delete-to-neighbour");
        app.presets
            .save_as(&Preset::default(), "Mine")
            .expect("saved");
        app.refresh_preset_list();
        pick(&mut app, "Mine");
        assert_eq!(
            app.settings.preset_for_device("alsa_output.speakers", OUT),
            Some("Mine")
        );
        app.handle(&[UiAction::DeletePreset]);
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
    }

    #[test]
    fn resetting_presets_selects_the_one_the_device_remembers() {
        // Upstream 7f160b6 (`FxController::resetPresets`).
        let mut app = speakers_on_alpha("reset-to-device");
        app.handle(&[UiAction::SetEffect(Effect::Bass, 6.0)]);
        app.settings.remember_device_preset(
            "alsa_output.speakers",
            "alsa_output.speakers",
            "Beta",
            "speaker",
            OUT,
        );
        let mut pane = app.settings_state();
        app.handle_settings(&SettingsAction::ResetPresets, &mut pane);
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
    }

    #[test]
    fn resetting_presets_with_no_device_memory_reloads_the_selected_one_as_saved() {
        let mut app = with_presets(&[]);
        let (store, _dir) = music_store();
        app.presets = store;
        app.refresh_preset_list();
        pick(&mut app, "Beta");
        app.handle(&[UiAction::SetEffect(Effect::Bass, 2.0)]);
        let mut pane = app.settings_state();
        app.handle_settings(&SettingsAction::ResetPresets, &mut pane);
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert!((app.params().effect(Effect::Bass) - 0.6).abs() < 0.01);
    }

    /// The saved run of [`saved_settings`] — the headphones on `Beta` — brought up to the moment the
    /// engine has attached them as asked, with the speakers remembering `Alpha`.
    fn on_the_headphones() -> (App, FakeEngine, tempfile::TempDir) {
        let mut settings = saved_settings(OUT);
        settings.remember_device_preset(SPEAKERS, "Speakers", "Alpha", "speaker", OUT);
        let (mut app, engine, dir) = started_with(settings);
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        let _ = app.drain_events();
        (app, engine, dir)
    }

    #[test]
    fn a_device_the_engine_moves_to_on_its_own_brings_back_the_preset_it_remembers() {
        // The headphones are unplugged and the engine's rules take the speakers: they come with
        // the preset they were last used with, as a pick in the window would bring it (U6).
        let (mut app, engine, _dir) = on_the_headphones();
        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
        assert_eq!(app.params().effect(Effect::Bass), 0.0, "Alpha's flat bass");
        assert!(
            app.drain_events().contains(&AppEvent::PresetChanged {
                direction: OUT,
                name: Some("Alpha".to_owned()),
                modified: false,
            }),
            "the stream hears it"
        );
    }

    #[test]
    fn a_device_the_window_asked_for_or_the_lane_already_had_moves_no_preset() {
        let (mut app, engine, _dir) = on_the_headphones();
        // The lane attached again to what it already had: no move.
        app.settings
            .remember_device_preset(HEADPHONES, "Headphones", "Alpha", "headphones", OUT);
        engine.feed(attached(OUT, Some(HEADPHONES)));
        app.poll_audio();
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));

        // Asked for from the window, which brought the preset with the pick; the engine's answer
        // brings nothing more.
        let speakers = app
            .state
            .devices
            .iter()
            .position(|d| d.name == SPEAKERS)
            .expect("listed");
        app.handle(&[UiAction::SelectOutput(speakers)]);
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
        app.settings
            .remember_device_preset(SPEAKERS, "Speakers", "Beta", "speaker", OUT);
        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
    }

    #[test]
    fn a_start_the_engine_attached_elsewhere_first_ends_on_the_saved_devices_preset() {
        // The engine attaches the output by its own rules before the list arrives: the speakers,
        // which bring Alpha. The saved headphones, announced once they are listed, bring back
        // their own Beta rather than playing on with the speakers' preset.
        let mut settings = saved_settings(OUT);
        settings.remember_device_preset(SPEAKERS, "Speakers", "Alpha", "speaker", OUT);
        settings.remember_device_preset(HEADPHONES, "Headphones", "Beta", "headphones", OUT);
        let (mut app, engine, _dir) = started_with(settings);
        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));

        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert!(
            engine.take_sent().contains(&UiToAudio::SelectDevice {
                node_name: HEADPHONES.to_owned(),
                direction: OUT,
            }),
            "the saved headphones are asked for"
        );
        engine.feed(attached(OUT, Some(HEADPHONES)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert_eq!(app.settings.output_preset, "Beta");
    }

    #[test]
    fn the_saved_device_plugged_back_in_brings_back_its_preset() {
        let (mut app, engine, _dir) = on_the_headphones();
        app.settings
            .remember_device_preset(HEADPHONES, "Headphones", "Beta", "headphones", OUT);
        // Unplugged: the engine takes the speakers, and their Alpha.
        engine.feed(AudioToUi::Devices(without_the_headphones()));
        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
        // Plugged back in: announced again, and back on Beta.
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
    }

    /// [`two_lane_devices`] with the headphones unplugged.
    fn without_the_headphones() -> Vec<AudioDevice> {
        two_lane_devices()
            .into_iter()
            .filter(|d| d.name != HEADPHONES)
            .collect()
    }

    /// The saved run of [`saved_settings`] — the headphones on `Beta`, which they remember — just
    /// started, with `Alpha` picked before the engine has said anything: from the command line,
    /// where `fxsound --preset Alpha` at a cold start runs before the device list, or from the
    /// window.
    fn alpha_picked_at_a_cold_start(from_the_window: bool) -> (App, FakeEngine, tempfile::TempDir) {
        let mut settings = saved_settings(OUT);
        settings.remember_device_preset(HEADPHONES, "Headphones", "Beta", "headphones", OUT);
        let (mut app, engine, dir) = started_with(settings);
        if from_the_window {
            pick(&mut app, "Alpha");
        } else {
            let outcome = crate::commands::run(
                &mut app,
                &[crate::cli::Command::Preset(PresetCommand::Select(
                    "Alpha".to_owned(),
                ))],
            );
            assert!(!outcome.failed, "{}", outcome.stderr);
        }
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
        let _ = engine.take_sent();
        (app, engine, dir)
    }

    #[test]
    fn a_preset_picked_at_a_cold_start_survives_the_saved_device_being_listed() {
        // The saved headphones are announced and then attached. The preset they remember is the
        // one the pick replaced: bringing it back would undo the pick, and upstream moves the
        // preset only for a device other than the one it shows (`FxController.cpp:1645-1658`).
        for from_the_window in [false, true] {
            let (mut app, engine, _dir) = alpha_picked_at_a_cold_start(from_the_window);
            engine.feed(AudioToUi::Devices(two_lane_devices()));
            app.poll_audio();
            assert!(
                engine.take_sent().contains(&UiToAudio::SelectDevice {
                    node_name: HEADPHONES.to_owned(),
                    direction: OUT,
                }),
                "the saved headphones are still asked for"
            );
            assert_eq!(
                app.lane_preset(OUT),
                Some(("Alpha", false)),
                "listed, picked from the window: {from_the_window}"
            );
            engine.feed(attached(OUT, Some(HEADPHONES)));
            app.poll_audio();
            assert_eq!(selected(&app, OUT), Some(HEADPHONES));
            assert_eq!(
                app.lane_preset(OUT),
                Some(("Alpha", false)),
                "attached, picked from the window: {from_the_window}"
            );
            assert_eq!(app.settings.output_preset, "Alpha");
        }
    }

    #[test]
    fn a_preset_picked_at_a_cold_start_survives_the_saved_device_attaching_before_the_list() {
        for from_the_window in [false, true] {
            let (mut app, engine, _dir) = alpha_picked_at_a_cold_start(from_the_window);
            engine.feed(attached(OUT, Some(HEADPHONES)));
            app.poll_audio();
            assert_eq!(
                app.lane_preset(OUT),
                Some(("Alpha", false)),
                "attached, picked from the window: {from_the_window}"
            );
            engine.feed(AudioToUi::Devices(two_lane_devices()));
            app.poll_audio();
            assert_eq!(selected(&app, OUT), Some(HEADPHONES));
            assert_eq!(
                app.lane_preset(OUT),
                Some(("Alpha", false)),
                "listed, picked from the window: {from_the_window}"
            );
            assert_eq!(app.settings.output_preset, "Alpha");
        }
    }

    #[test]
    fn a_lane_back_on_the_device_it_was_on_after_a_reconnect_has_not_moved() {
        // The headphones went and the engine took the speakers, with their Alpha. Then the
        // connection dropped, and Beta was picked while the lane had no nodes: the speakers
        // coming back are the device the lane was on, not a move, and the pick stays.
        let (mut app, engine, _dir) = on_the_headphones();
        engine.feed(AudioToUi::Devices(without_the_headphones()));
        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));

        engine.feed(AudioToUi::Disconnected {
            reason: "the server went away".to_owned(),
        });
        engine.feed(attached(OUT, None));
        app.poll_audio();
        pick(&mut app, "Beta");
        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
    }

    #[test]
    fn the_saved_device_listed_again_while_the_lane_has_no_nodes_still_brings_its_preset() {
        // The lane was last on the speakers, with their Alpha, when the connection dropped; the
        // headphones are back in the first list after it. That is a move, nodes or no nodes.
        let (mut app, engine, _dir) = on_the_headphones();
        app.settings
            .remember_device_preset(HEADPHONES, "Headphones", "Beta", "headphones", OUT);
        engine.feed(AudioToUi::Devices(without_the_headphones()));
        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        assert_eq!(app.lane_preset(OUT), Some(("Alpha", false)));
        let _ = engine.take_sent();

        engine.feed(AudioToUi::Disconnected {
            reason: "the server went away".to_owned(),
        });
        engine.feed(attached(OUT, None));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert!(
            engine.take_sent().contains(&UiToAudio::SelectDevice {
                node_name: HEADPHONES.to_owned(),
                direction: OUT,
            }),
            "the saved headphones are asked for again"
        );
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
    }

    #[test]
    fn a_microphone_the_engine_moves_to_brings_its_voice_preset_without_moving_the_window() {
        const USB: &str = "alsa_input.usb";
        let mut settings = saved_settings(OUT);
        settings.remember_device_preset(USB, "USB microphone", "Quiet", "microphone", IN);
        let (mut app, engine, _dir) = started_with(settings);
        let mut devices = two_lane_devices();
        devices.push(device(USB, IN, false));
        engine.feed(AudioToUi::Devices(devices));
        engine.feed(attached(OUT, Some(HEADPHONES)));
        engine.feed(attached(IN, Some(MIC)));
        app.poll_audio();
        assert_eq!(app.lane_preset(IN), Some(("Loud", false)));
        assert_eq!(app.input_params().makeup_db, 9.0);

        engine.feed(attached(IN, Some(USB)));
        app.poll_audio();
        assert_eq!(app.state.direction, OUT, "the window stays on the speakers");
        assert_eq!(app.lane_preset(IN), Some(("Quiet", false)));
        assert_eq!(app.input_params().makeup_db, 0.0);
        assert_eq!(app.settings.input_preset, "Quiet");
    }

    fn listed_entry(name: &str, factory: bool, modified: bool) -> PresetEntry {
        PresetEntry {
            name: name.to_owned(),
            factory,
            modified,
        }
    }

    /// A headless app listing the factory `Jazz` and the user's `Mine`, with one of them selected.
    fn choosing(factory: bool, modified: bool) -> App {
        let mut app = headless();
        app.state.presets = vec![
            listed_entry("Jazz", true, false),
            listed_entry("Mine", false, false),
        ];
        let at = usize::from(!factory);
        app.state.presets[at].modified = modified;
        app.state.selected_preset = Some(at);
        app
    }

    #[test]
    fn the_menu_offers_a_preset_item_exactly_when_its_command_would_run() {
        // One rule for the hamburger and the command path (U15), and it is still the original's
        // enablement (`FxMainWindow.cpp:536-543`).
        for factory in [true, false] {
            for modified in [false, true] {
                for power in [true, false] {
                    let mut app = choosing(factory, modified);
                    app.state.power = power;
                    let menu = app.preset_menu();
                    let runs = |command: P| match app.preset_command_allowed(&command) {
                        Ok(()) => true,
                        Err(refusal) => refusal.is_about_the_name(),
                    };
                    let case = format!("factory {factory}, modified {modified}, power {power}");
                    assert_eq!(
                        menu.save_new,
                        power && runs(P::SaveAs(String::new())),
                        "{case}"
                    );
                    assert_eq!(menu.overwrite, power && runs(P::Overwrite), "{case}");
                    assert_eq!(menu.undo, power && runs(P::Undo), "{case}");
                    assert_eq!(
                        menu.rename,
                        power && runs(P::Rename(String::new())),
                        "{case}"
                    );
                    assert_eq!(menu.delete, power && runs(P::Delete), "{case}");

                    assert_eq!(menu.save_new, modified && power, "{case}");
                    assert_eq!(menu.overwrite, modified && !factory && power, "{case}");
                    assert_eq!(menu.undo, modified && power, "{case}");
                    assert_eq!(menu.rename, !modified && !factory && power, "{case}");
                    assert_eq!(menu.delete, !factory && power, "{case}");
                }
            }
        }
    }

    #[test]
    fn each_refusal_says_what_stands_in_the_way() {
        let refused = |app: &App, command: P| app.preset_command_allowed(&command).unwrap_err();
        let jazz = || "Jazz".to_owned();
        let mine = || "Mine".to_owned();

        let app = choosing(true, true);
        assert_eq!(
            refused(&app, P::Overwrite),
            Refusal::FactoryOverwrite { preset: jazz() }
        );
        assert_eq!(
            refused(&app, P::Rename("New".into())),
            Refusal::FactoryRename { preset: jazz() }
        );
        assert_eq!(
            refused(&app, P::Delete),
            Refusal::FactoryDelete { preset: jazz() }
        );
        assert_eq!(
            refused(&app, P::SaveAs("mine".into())),
            Refusal::NameTaken {
                name: "mine".into()
            },
            "names are compared ignoring case"
        );
        assert_eq!(refused(&app, P::SaveAs("  ".into())), Refusal::EmptyName);
        assert_eq!(app.preset_command_allowed(&P::SaveAs("New".into())), Ok(()));
        assert_eq!(app.preset_command_allowed(&P::Undo), Ok(()));

        let app = choosing(false, false);
        assert_eq!(
            refused(&app, P::Overwrite),
            Refusal::NothingToSave { preset: mine() }
        );
        assert_eq!(
            refused(&app, P::SaveAs("New".into())),
            Refusal::NothingToSave { preset: mine() }
        );
        assert_eq!(
            refused(&app, P::Undo),
            Refusal::NothingToUndo { preset: mine() }
        );
        assert_eq!(
            refused(&app, P::Rename("Mine".into())),
            Refusal::NameTaken { name: mine() }
        );
        assert_eq!(
            app.preset_command_allowed(&P::Rename("Yours".into())),
            Ok(())
        );
        assert_eq!(app.preset_command_allowed(&P::Delete), Ok(()));

        let app = choosing(false, true);
        assert_eq!(
            refused(&app, P::Rename("Yours".into())),
            Refusal::UnsavedChanges { preset: mine() }
        );
        assert_eq!(app.preset_command_allowed(&P::Overwrite), Ok(()));

        let mut app = choosing(false, true);
        app.state.selected_preset = None;
        for command in [
            P::SaveAs("New".into()),
            P::Overwrite,
            P::Undo,
            P::Rename("New".into()),
            P::Delete,
        ] {
            assert_eq!(refused(&app, command), Refusal::NoPresetSelected);
        }
        assert_eq!(
            refused(&app, P::Select("Nope".into())),
            Refusal::UnknownPreset {
                lane: OUT,
                name: "Nope".into(),
                other_lane_has_it: false,
            }
        );
        assert_eq!(
            app.preset_command_allowed(&P::Select("Jazz".into())),
            Ok(())
        );
        assert_eq!(app.preset_command_allowed(&P::Next), Ok(()));
        assert_eq!(app.preset_command_allowed(&P::Previous), Ok(()));
    }

    #[test]
    fn a_refusal_reads_as_the_reason_and_the_way_round_it() {
        let text = |refusal: Refusal| refusal.to_string();
        assert_eq!(
            text(Refusal::FactoryOverwrite {
                preset: "Jazz".into()
            }),
            "\"Jazz\" is a factory preset and cannot be overwritten; save the changes as a new \
             preset with --save_preset"
        );
        assert!(
            text(Refusal::UnsavedChanges {
                preset: "Mine".into()
            })
            .contains("--undo_preset")
        );
        assert_eq!(
            text(Refusal::NameTaken {
                name: "Rock".into()
            }),
            "a preset called \"Rock\" already exists"
        );
        assert_eq!(
            text(Refusal::LimitReached { max: 10 }),
            "the limit of 10 user presets is reached; delete one before saving another"
        );
        assert_eq!(
            text(Refusal::UnknownPreset {
                lane: OUT,
                name: "Loud".into(),
                other_lane_has_it: true,
            }),
            "no output preset is called \"Loud\"; it is an input preset, so add --edit=input to \
             select it"
        );
        assert!(!Refusal::NoPresetSelected.is_about_the_name());
        assert!(Refusal::EmptyName.is_about_the_name());
    }

    #[test]
    fn save_new_is_refused_at_the_user_preset_cap_and_the_menu_greys_it_out() {
        let mut app = choosing(true, true);
        app.settings.max_user_presets = 10;
        for n in 1..9 {
            app.state
                .presets
                .push(listed_entry(&format!("Mine {n}"), false, false));
        }
        assert_eq!(app.user_preset_count(), 9);
        assert_eq!(app.preset_command_allowed(&P::SaveAs("New".into())), Ok(()));
        assert!(app.preset_menu().save_new);

        app.state.presets.push(listed_entry("Mine 9", false, false));
        assert_eq!(
            app.preset_command_allowed(&P::SaveAs("New".into())),
            Err(Refusal::LimitReached { max: 10 })
        );
        assert!(!app.preset_menu().save_new);
    }

    #[test]
    fn exporting_from_the_window_writes_the_saved_preset_not_its_unsaved_edits() {
        // Upstream PR #155 (U16): the edits live in the autosave until they are saved.
        let (mut app, dir) = with_store();
        add_user_preset(&mut app, "Music");
        add_user_preset(&mut app, "Other");
        pick(&mut app, "Music");
        app.handle(&[UiAction::SetEffect(Effect::Bass, 8.0)]);
        // Switching away stashes the edits in the autosave, which the store prefers on load.
        pick(&mut app, "Other");
        let (stashed, from_autosave) = app.presets.load("Music").expect("load");
        assert!(from_autosave);
        assert!(
            stashed.effect(Effect::Bass) > 0.5,
            "the edits are in the autosave"
        );

        let mut state = ExportState {
            presets: vec!["Music".into()],
            selected: [0].into_iter().collect(),
            ..ExportState::default()
        };
        app.handle_export(&PresetsAction::Export, &mut state);
        assert_eq!(state.finished, Some(true));
        let exported =
            fxsound_preset::load(&dir.path().join("export/Music.fac")).expect("the export");
        assert_eq!(
            exported.effect(Effect::Bass),
            0.0,
            "as saved, not as edited"
        );
    }

    #[test]
    fn exporting_a_voice_preset_writes_it_as_saved_too() {
        let (mut app, dir) = with_store();
        let voices_dir = use_voices(
            &mut app,
            &[fxsound_preset::input::InputPreset {
                name: "Loud".to_owned(),
                makeup_db: 9.0,
                ..fxsound_preset::input::InputPreset::default()
            }],
        );
        app.handle(&[UiAction::SetEditDirection(IN)]);
        pick(&mut app, "Loud");
        app.handle(&[UiAction::SetMasterGain(-4.0)]);
        let edited = app.lane_snapshot(IN).expect("a voice preset").0;
        app.autosave_lane_preset(&edited);

        let mut state = ExportState {
            presets: vec!["Loud".into()],
            selected: [0].into_iter().collect(),
            ..ExportState::default()
        };
        app.handle_export(&PresetsAction::Export, &mut state);
        assert_eq!(state.finished, Some(true));
        let exported = InputPreset::load(&dir.path().join("export/Loud.toml")).expect("the export");
        assert_eq!(exported.makeup_db, 9.0, "as saved, not as edited");
        drop(voices_dir);
    }

    // ---- the device priority list (U4) ----------------------------------------------------------

    const DOCK: &str = "alsa_output.dock";

    fn rankings(sent: &[UiToAudio]) -> Vec<(DeviceDirection, Vec<String>)> {
        sent.iter()
            .filter_map(|message| match message {
                UiToAudio::SetDevicePriority { direction, names } => {
                    Some((*direction, names.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn owned(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn listed(app: &App) -> Vec<&str> {
        app.state.devices.iter().map(|d| d.name.as_str()).collect()
    }

    /// A start from `settings` that has had the two-lane device list, and the engine's answer
    /// that the speakers' lane is on `output`.
    fn listed_with(settings: Settings, output: &str) -> (App, FakeEngine, tempfile::TempDir) {
        let (mut app, engine, dir) = started_with(settings);
        engine.feed(attached(OUT, Some(output)));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        (app, engine, dir)
    }

    #[test]
    fn the_ranking_the_settings_file_holds_goes_to_the_engine_at_start() {
        let mut settings = saved_settings(OUT);
        settings.remember_device_preset(SPEAKERS, "Speakers", "", "", OUT);
        settings.remember_device_preset(MIC, "Mic", "", "", IN);
        settings.remember_device_preset(HEADPHONES, "Headphones", "", "", OUT);
        let messages = startup_messages(&settings);
        assert_eq!(
            rankings(&messages),
            [(OUT, owned(&[SPEAKERS, HEADPHONES])), (IN, owned(&[MIC]))]
        );
    }

    #[test]
    fn a_device_list_puts_every_device_on_the_priority_list_and_the_engine_is_told_first() {
        let (mut app, engine, _dir) = started_with(saved_settings(OUT));
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        let sent = engine.take_sent();
        // The saved device first, then the rest as listed; the microphone on its own list.
        assert_eq!(
            rankings(&sent),
            [(OUT, owned(&[HEADPHONES, SPEAKERS])), (IN, owned(&[MIC]))]
        );
        let first_select = sent
            .iter()
            .position(|m| matches!(m, UiToAudio::SelectDevice { .. }))
            .expect("the saved devices are announced");
        let last_ranking = sent
            .iter()
            .rposition(|m| matches!(m, UiToAudio::SetDevicePriority { .. }))
            .expect("rankings");
        assert!(
            last_ranking < first_select,
            "ranked before any lane is attached: {sent:?}"
        );
        assert_eq!(app.settings.device_configs.len(), 3);
        assert!(
            app.settings_dirty,
            "the list is written to the settings file"
        );
        assert_eq!(app.settings.preset_for_device(SPEAKERS, OUT), None);

        // The same list again teaches nothing and says nothing.
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        assert!(rankings(&engine.take_sent()).is_empty());
    }

    #[test]
    fn the_combos_and_the_tray_list_devices_in_their_rank_order() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        assert_eq!(listed(&app), [HEADPHONES, SPEAKERS, MIC]);
        let tray: Vec<String> = app
            .tray_state()
            .devices
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(tray, [HEADPHONES, SPEAKERS, MIC]);
        let _ = engine.take_sent();
        let _ = app.drain_events();
        let _ = app.take_tray_refresh();

        // The speakers moved to the top in Settings: the lists follow, the engine is told, and the
        // lane stays where it is, as dragging a row of upstream's list moves nothing.
        let mut pane = app.settings_state();
        app.handle_settings(&SettingsAction::MoveDeviceUp(1), &mut pane);
        assert_eq!(listed(&app), [SPEAKERS, HEADPHONES, MIC]);
        // No device came or went, so the stream hears nothing; the tray is redrawn in the new order.
        assert_eq!(app.drain_events(), []);
        assert!(app.take_tray_refresh());
        assert_eq!(
            engine.take_sent(),
            [UiToAudio::SetDevicePriority {
                direction: OUT,
                names: owned(&[SPEAKERS, HEADPHONES]),
            }]
        );
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        assert_eq!(
            pane.devices
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [SPEAKERS, HEADPHONES]
        );
        assert_eq!(
            pane.selected_device,
            Some(0),
            "the moved row stays selected"
        );
        let tray: Vec<String> = app
            .tray_state()
            .devices
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(tray, [SPEAKERS, HEADPHONES, MIC]);
    }

    #[test]
    fn a_new_device_goes_to_the_bottom_and_takes_nothing() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let _ = engine.take_sent();
        let mut more = two_lane_devices();
        more.push(device(DOCK, OUT, false));
        engine.feed(AudioToUi::Devices(more));
        app.poll_audio();
        let sent = engine.take_sent();
        assert_eq!(
            rankings(&sent),
            [(OUT, owned(&[HEADPHONES, SPEAKERS, DOCK]))]
        );
        assert!(select_devices(&sent).is_empty(), "{sent:?}");
        assert_eq!(listed(&app), [HEADPHONES, SPEAKERS, DOCK, MIC]);
    }

    #[test]
    fn with_new_devices_prioritised_a_newcomer_goes_to_the_top_and_takes_its_lane() {
        let mut settings = saved_settings(OUT);
        settings.prioritize_new_output = true;
        let (mut app, engine, _dir) = listed_with(settings, HEADPHONES);
        let _ = engine.take_sent();
        let mut more = two_lane_devices();
        more.push(device(DOCK, OUT, false));
        engine.feed(AudioToUi::Devices(more));
        app.poll_audio();
        let sent = engine.take_sent();
        // Ranked first, then asked for: the engine ran its rules before the ranking named it.
        assert_eq!(
            sent,
            [
                UiToAudio::SetDevicePriority {
                    direction: OUT,
                    names: owned(&[DOCK, HEADPHONES, SPEAKERS]),
                },
                select(DOCK, OUT),
            ]
        );
        assert_eq!(
            selected(&app, OUT),
            Some(DOCK),
            "shown while it is asked for"
        );
        assert_eq!(
            app.settings.device_name(OUT),
            DOCK,
            "as upstream's setOutput saves it"
        );
        assert_eq!(app.state.direction, OUT);
        // The engine's answer is the move the lane asked for: no preset comes with it.
        engine.feed(attached(OUT, Some(DOCK)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(DOCK));
        // And the next list does not ask again.
        let mut again = two_lane_devices();
        again.push(device(DOCK, OUT, false));
        engine.feed(AudioToUi::Devices(again));
        app.poll_audio();
        assert!(select_devices(&engine.take_sent()).is_empty());
    }

    #[test]
    fn a_prioritised_newcomer_moves_no_lane_that_is_off_or_follows_the_system() {
        for (lane_off, following) in [(true, false), (false, true)] {
            let mut settings = saved_settings(OUT);
            settings.prioritize_new_output = true;
            settings.follow_system_default = following;
            settings.set_lane_enabled(IN, !lane_off);
            let (mut app, engine, _dir) = listed_with(settings, HEADPHONES);
            let _ = engine.take_sent();
            let mut more = two_lane_devices();
            more.push(device("alsa_input.webcam", IN, false));
            engine.feed(AudioToUi::Devices(more));
            app.poll_audio();
            let sent = engine.take_sent();
            assert!(
                select_devices(&sent).is_empty(),
                "lane off {lane_off}, following {following}: {sent:?}"
            );
            // Still at the top of the microphones' list, for when the lane or the list is back.
            assert_eq!(
                priority::ranked(&app.settings, IN)
                    .map(|config| config.device_id.as_str())
                    .collect::<Vec<_>>(),
                ["alsa_input.webcam", MIC]
            );
        }
    }

    #[test]
    fn following_the_systems_default_gives_the_engine_no_ranking_until_it_is_switched_back() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let _ = engine.take_sent();
        let mut pane = app.settings_state();
        app.handle_settings(&SettingsAction::SetFollowSystemDefault(true), &mut pane);
        assert!(app.settings.follow_system_default);
        assert!(pane.settings.follow_system_default);
        assert_eq!(
            rankings(&engine.take_sent()),
            [(OUT, Vec::new()), (IN, Vec::new())]
        );
        // The list itself is kept, and still orders the combos.
        assert_eq!(app.settings.device_configs.len(), 3);
        assert_eq!(listed(&app), [HEADPHONES, SPEAKERS, MIC]);
        assert!(startup_messages(&app.settings).iter().all(
            |m| !matches!(m, UiToAudio::SetDevicePriority { names, .. } if !names.is_empty())
        ));

        app.handle_settings(&SettingsAction::SetFollowSystemDefault(false), &mut pane);
        assert_eq!(
            rankings(&engine.take_sent()),
            [(OUT, owned(&[HEADPHONES, SPEAKERS])), (IN, owned(&[MIC]))]
        );
    }

    #[test]
    fn the_microphone_pane_ranks_the_microphones() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let mut more = two_lane_devices();
        more.push(device("alsa_input.webcam", IN, false));
        engine.feed(AudioToUi::Devices(more));
        app.poll_audio();
        let _ = engine.take_sent();

        let mut pane = app.settings_state();
        let rows = |pane: &SettingsState| -> Vec<String> {
            pane.microphones.iter().map(|row| row.id.clone()).collect()
        };
        assert_eq!(rows(&pane), [MIC, "alsa_input.webcam"]);
        assert!(pane.microphones.iter().all(|row| row.preset.is_none()));
        assert!(pane.microphones.iter().all(|row| row.present));

        app.handle_settings(&SettingsAction::MoveMicrophoneDown(0), &mut pane);
        assert_eq!(rows(&pane), ["alsa_input.webcam", MIC]);
        assert_eq!(
            engine.take_sent(),
            [UiToAudio::SetDevicePriority {
                direction: IN,
                names: owned(&["alsa_input.webcam", MIC]),
            }]
        );
        assert_eq!(
            listed(&app),
            [HEADPHONES, SPEAKERS, "alsa_input.webcam", MIC]
        );
        // The speakers' list is not touched by it.
        assert_eq!(
            pane.devices
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [HEADPHONES, SPEAKERS]
        );

        app.handle_settings(&SettingsAction::MoveMicrophoneUp(1), &mut pane);
        assert_eq!(rows(&pane), [MIC, "alsa_input.webcam"]);

        // A microphone that is gone can be forgotten; the speakers' rows keep their numbers.
        engine.feed(AudioToUi::Devices(two_lane_devices()));
        app.poll_audio();
        app.refresh_settings_state(&mut pane);
        assert!(!pane.microphones[1].present);
        app.handle_settings(&SettingsAction::RemoveMicrophone(1), &mut pane);
        assert_eq!(rows(&pane), [MIC]);
        assert_eq!(pane.devices.len(), 2);
    }

    #[test]
    fn a_device_arriving_while_the_pane_is_open_joins_its_list_there() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let mut pane = app.settings_state();
        assert_eq!(pane.devices.len(), 2);
        let mut more = two_lane_devices();
        more.push(device(DOCK, OUT, false));
        engine.feed(AudioToUi::Devices(more));
        app.poll_audio();
        app.refresh_settings_state(&mut pane);
        assert_eq!(
            pane.devices
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            [HEADPHONES, SPEAKERS, DOCK]
        );
        assert_eq!(pane.settings.device_configs, app.settings.device_configs);
    }

    #[test]
    fn a_clicked_row_is_the_one_shift_arrows_move() {
        let (mut app, _engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let mut pane = app.settings_state();
        app.handle_settings(&SettingsAction::SelectDeviceRow(1), &mut pane);
        assert_eq!(pane.selected_device, Some(1));
        app.handle_settings(&SettingsAction::MoveDeviceUp(1), &mut pane);
        assert_eq!(pane.selected_device, Some(0));
        app.handle_settings(&SettingsAction::MoveDeviceDown(0), &mut pane);
        assert_eq!(pane.selected_device, Some(1));
        // Past the end moves nothing and keeps the selection.
        app.handle_settings(&SettingsAction::MoveDeviceDown(1), &mut pane);
        assert_eq!(pane.selected_device, Some(1));
        app.handle_settings(&SettingsAction::SelectDeviceRow(7), &mut pane);
        assert_eq!(pane.selected_device, None);
    }

    // ---- power off hands the default back (U12) ----------------------------------------------

    fn default_wishes(sent: &[UiToAudio]) -> Vec<(DeviceDirection, bool)> {
        sent.iter()
            .filter_map(|message| match message {
                UiToAudio::SetAsDefault { direction, want } => Some((*direction, *want)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn power_off_hands_both_defaults_back_and_power_on_takes_them_again() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let _ = engine.take_sent();
        app.handle(&[UiAction::TogglePower]);
        assert!(!app.state.power);
        assert_eq!(
            default_wishes(&engine.take_sent()),
            [(OUT, false), (IN, false)]
        );
        assert!(!engine.params().expect("published").power, "and bypassed");
        app.handle(&[UiAction::TogglePower]);
        assert_eq!(
            default_wishes(&engine.take_sent()),
            [(OUT, true), (IN, true)]
        );
    }

    #[test]
    fn while_off_each_combo_shows_the_systems_default_device() {
        // The speakers are the default sink, the microphone the default source; FxSound's output
        // lane is on the headphones.
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        engine.feed(attached(IN, Some(MIC)));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));

        app.handle(&[UiAction::TogglePower]);
        assert_eq!(
            selected(&app, OUT),
            Some(SPEAKERS),
            "where the sound goes now"
        );
        assert_eq!(selected(&app, IN), Some(MIC));

        // The desktop's own settings pick the headphones: the combo follows.
        let mut list = two_lane_devices();
        for device in &mut list {
            if device.direction == OUT {
                device.is_default = device.name == HEADPHONES;
            }
        }
        engine.feed(AudioToUi::Devices(list));
        app.poll_audio();
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));

        app.handle(&[UiAction::TogglePower]);
        assert_eq!(selected(&app, OUT), Some(HEADPHONES), "FxSound's own again");
    }

    #[test]
    fn a_device_picked_while_off_shows_until_the_engine_attaches_it() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        app.handle(&[UiAction::TogglePower]);
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        let _ = engine.take_sent();

        app.handle(&[UiAction::SelectOutput(device_at(&app, HEADPHONES))]);
        // Already attached there, so nothing is waited for: the system's default is shown.
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        engine.feed(attached(OUT, Some(SPEAKERS)));
        app.poll_audio();
        app.handle(&[UiAction::SelectOutput(device_at(&app, HEADPHONES))]);
        assert_eq!(
            selected(&app, OUT),
            Some(HEADPHONES),
            "asked for, not answered"
        );
        assert_eq!(select_devices(&engine.take_sent()).len(), 2);
        engine.feed(attached(OUT, Some(HEADPHONES)));
        app.poll_audio();
        assert_eq!(
            selected(&app, OUT),
            Some(SPEAKERS),
            "answered: the sound goes to the system's default until the power is on"
        );
        // And it is the device the lane is on when the power comes back.
        app.handle(&[UiAction::TogglePower]);
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
    }

    #[test]
    fn a_pick_while_off_brings_its_preset_to_the_pick_and_not_to_the_default() {
        let (mut app, _engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        app.settings
            .remember_device_preset(HEADPHONES, "Headphones", "Alpha", "", OUT);
        app.handle(&[UiAction::TogglePower]);
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        let _ = app.drain_events();

        app.handle(&[UiAction::SelectOutput(device_at(&app, HEADPHONES))]);
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Alpha"));
        assert_eq!(
            app.settings.preset_for_device(HEADPHONES, OUT),
            Some("Alpha")
        );
        assert_eq!(
            app.settings.preset_for_device(SPEAKERS, OUT),
            None,
            "the default device shown is not what was picked"
        );
        assert_eq!(selected(&app, OUT), Some(SPEAKERS));
        assert!(
            !app.drain_events()
                .iter()
                .any(|event| matches!(event, AppEvent::DeviceChanged { .. })),
            "the combo never left the default, so there is nothing to say"
        );
    }

    #[test]
    fn a_start_with_the_power_off_hands_the_defaults_back_before_any_device_is_attached() {
        let mut settings = saved_settings(OUT);
        settings.power = false;
        let messages = startup_messages(&settings);
        assert_eq!(default_wishes(&messages), [(OUT, false), (IN, false)]);
        assert!(select_devices(&messages).is_empty());
        // A start with the power on says nothing about the default: the engine wants it anyway.
        assert!(default_wishes(&startup_messages(&saved_settings(OUT))).is_empty());
    }

    // ---- sleep (U13) ------------------------------------------------------------------------------

    #[test]
    fn going_to_sleep_mutes_both_lanes_and_tells_the_engine() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let _ = engine.take_sent();
        app.system_sleeping(true);
        assert!(app.is_system_sleeping());
        assert_eq!(engine.take_sent(), [UiToAudio::SystemSleeping(true)]);
        assert!(engine.params().expect("published").mute);
        assert!(engine.input_params().expect("published").mute);
        assert!(
            engine.take_events().is_empty(),
            "nothing is reset on the way down"
        );
    }

    #[test]
    fn resuming_clears_both_lanes_filters_and_unmutes_them() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        app.system_sleeping(true);
        let _ = engine.take_sent();
        let _ = engine.take_events();
        app.system_sleeping(false);
        assert!(!app.is_system_sleeping());
        assert_eq!(engine.take_sent(), [UiToAudio::SystemSleeping(false)]);
        assert_eq!(
            engine.take_events(),
            [
                (OUT, DspEvent::ResetFilterState),
                (IN, DspEvent::ResetFilterState)
            ]
        );
        assert!(!engine.params().expect("published").mute);
        assert!(!engine.input_params().expect("published").mute);
    }

    #[test]
    fn nothing_the_user_moves_while_the_system_sleeps_unmutes_it() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        app.system_sleeping(true);
        app.handle(&[UiAction::SetEffect(Effect::Bass, 7.0)]);
        app.handle(&[UiAction::TogglePower]);
        app.handle(&[UiAction::TogglePower]);
        app.handle(&[UiAction::SetEditDirection(IN)]);
        app.handle(&[UiAction::SetMasterGain(3.0)]);
        assert!(engine.params().expect("published").mute);
        assert!(engine.input_params().expect("published").mute);
    }

    #[test]
    fn the_same_word_twice_is_said_once() {
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let _ = engine.take_sent();
        app.system_sleeping(false);
        assert!(engine.take_sent().is_empty(), "awake is where it starts");
        app.system_sleeping(true);
        app.system_sleeping(true);
        assert_eq!(engine.take_sent(), [UiToAudio::SystemSleeping(true)]);
    }

    #[test]
    fn loginds_signal_reaches_the_engine_as_the_system_sleeping() {
        // The whole mapping, without a bus: the signal as logind sends it, read by the watcher,
        // handed to the controller, and what the engine is told.
        let (mut app, engine, _dir) = listed_with(saved_settings(OUT), HEADPHONES);
        let _ = engine.take_sent();
        for sleeping in [true, false] {
            let signal = zbus::Message::signal(
                crate::sleep::LOGIND_PATH,
                crate::sleep::LOGIND_MANAGER,
                crate::sleep::PREPARE_FOR_SLEEP,
            )
            .expect("a header")
            .build(&sleeping)
            .expect("a signal");
            let word = crate::sleep::prepare_for_sleep(&signal).expect("logind's signal");
            app.system_sleeping(word);
            assert_eq!(engine.take_sent(), [UiToAudio::SystemSleeping(sleeping)]);
            assert_eq!(engine.params().expect("published").mute, sleeping);
        }
    }

    // ---- the tray's two lanes and the pump's pacing (0.4.0 design §1.4, §12) ------------------

    /// [`listed_with`] on the headphones, the microphone attached as well.
    fn both_lanes_attached() -> (App, FakeEngine, tempfile::TempDir) {
        let (mut app, engine, dir) = listed_with(saved_settings(OUT), HEADPHONES);
        engine.feed(attached(IN, Some(MIC)));
        app.poll_audio();
        let _ = engine.take_sent();
        let _ = engine.take_events();
        let _ = app.drain_events();
        let _ = app.take_tray_refresh();
        (app, engine, dir)
    }

    fn tray_names(lane: &crate::tray::TrayLane) -> Vec<&str> {
        lane.presets.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn the_tray_draws_both_lanes_each_with_its_presets_its_tick_and_its_device() {
        let (app, _engine, _dir) = both_lanes_attached();
        let tray = app.tray_state();
        assert_eq!(tray_names(&tray.output), ["Alpha", "Beta"]);
        assert_eq!(tray.output.selected_preset, Some(1), "Beta");
        assert_eq!(tray_names(&tray.input), ["Loud", "Quiet"]);
        assert_eq!(tray.input.selected_preset, Some(0), "Loud, off screen");
        assert_eq!(tray.output.device, Some(device_at(&app, HEADPHONES)));
        assert_eq!(tray.input.device, Some(device_at(&app, MIC)));
        assert_eq!(
            tray.device(IN).map(|d| d.name.as_str()),
            Some(MIC),
            "the index is into the tray's own list, the window's"
        );
    }

    #[test]
    fn a_voice_preset_picked_in_the_tray_changes_the_microphone_and_leaves_the_window_where_it_was()
    {
        let (mut app, engine, _dir) = both_lanes_attached();
        assert_eq!(app.state.direction, OUT);
        app.handle_tray(crate::tray::TrayCommand::SelectPreset {
            direction: IN,
            name: "Quiet".to_owned(),
        });
        assert_eq!(app.lane_preset(IN), Some(("Quiet", false)));
        assert_eq!(
            app.lane_preset(OUT),
            Some(("Beta", false)),
            "the speakers keep theirs"
        );
        assert_eq!(
            app.state.direction, OUT,
            "the window still shows the speakers"
        );
        assert_eq!(app.settings().device_direction, OUT);
        assert_eq!(
            engine.input_params().expect("published").highpass_hz,
            75.0,
            "the microphone's chain runs the new voice at once"
        );
        let events = app.drain_events();
        assert!(
            events.contains(&AppEvent::PresetChanged {
                direction: IN,
                name: Some("Quiet".to_owned()),
                modified: false,
            }),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AppEvent::Direction { .. })),
            "{events:?}"
        );
        assert_eq!(app.tray_state().input.selected_preset, Some(1));
    }

    #[test]
    fn a_music_preset_picked_in_the_tray_is_the_window_s_own_pick() {
        let (mut app, _engine, _dir) = both_lanes_attached();
        app.handle_tray(crate::tray::TrayCommand::SelectPreset {
            direction: OUT,
            name: "Alpha".to_owned(),
        });
        assert_eq!(app.state.preset().map(|p| p.name.as_str()), Some("Alpha"));
        assert_eq!(app.lane_preset(IN), Some(("Loud", false)));
    }

    #[test]
    fn a_preset_the_lane_no_longer_has_changes_nothing() {
        let (mut app, engine, _dir) = both_lanes_attached();
        let params = engine.input_params();
        app.handle_tray(crate::tray::TrayCommand::SelectPreset {
            direction: IN,
            name: "Beta".to_owned(),
        });
        assert_eq!(app.lane_preset(IN), Some(("Loud", false)));
        assert_eq!(app.lane_preset(OUT), Some(("Beta", false)));
        assert_eq!(engine.input_params(), params);
        assert_eq!(app.state.direction, OUT);
    }

    #[test]
    fn off_in_the_tray_detaches_that_lane_and_leaves_the_other_alone() {
        let (mut app, engine, _dir) = both_lanes_attached();
        app.handle_tray(crate::tray::TrayCommand::Detach(IN));
        assert_eq!(app.state.selected_input, None);
        assert_eq!(selected(&app, OUT), Some(HEADPHONES));
        assert!(engine.take_sent().contains(&UiToAudio::DetachLane(IN)));
        let tray = app.tray_state();
        assert_eq!(tray.input.device, None);
        assert_eq!(
            tray.device_line(IN),
            format!("{}{}", tr("Input: "), tr("Off"))
        );
    }

    #[test]
    fn a_tray_pick_is_the_later_word_on_a_lane_a_command_line_left_waiting() {
        let (mut app, _engine, _dir) = both_lanes_attached();
        app.select_device_when_listed("usb-headset-not-yet-listed", OUT);
        app.select_device_when_listed("usb-mic-not-yet-listed", IN);
        app.handle_tray(crate::tray::TrayCommand::SelectDevice(device_at(
            &app, SPEAKERS,
        )));
        assert_eq!(app.pending_device(OUT), None, "the tray's pick stands");
        assert_eq!(
            app.pending_device(IN),
            Some("usb-mic-not-yet-listed"),
            "the other lane still waits"
        );
        app.handle_tray(crate::tray::TrayCommand::Detach(IN));
        assert_eq!(app.pending_device(IN), None, "and Off is a word too");
    }

    #[test]
    fn a_notice_is_the_next_deadline_until_it_is_taken_down() {
        let mut app = headless();
        assert_eq!(app.next_deadline(), None, "nothing is due");
        let before = Instant::now();
        app.raise_notice("Saved");
        let due = app.next_deadline().expect("the notice's end");
        assert!(due >= before + fxsound_ui::state::NOTICE_LIFETIME);
        assert!(due <= Instant::now() + fxsound_ui::state::NOTICE_LIFETIME);
        app.handle(&[UiAction::DismissNotice]);
        assert_eq!(app.next_deadline(), None);

        // A notice written straight into the state has no clock yet: the next poll is due now.
        app.state.notification = Some("Unstamped".to_owned());
        assert!(app.next_deadline().expect("due") <= Instant::now());
        app.poll_audio();
        assert!(app.next_deadline().expect("due") > Instant::now());
    }

    #[test]
    fn the_volumes_delayed_save_is_a_deadline_too() {
        let mut app = headless();
        let due = Instant::now() + std::time::Duration::from_millis(700);
        app.volume_save_due = Some(due);
        assert_eq!(app.next_deadline(), Some(due));
    }

    #[test]
    fn meters_moved_says_whether_the_shown_lane_s_meters_changed_since_the_last_poll() {
        let (mut app, engine, _dir) = both_lanes_attached();
        let playing = |level: f32| Meters {
            active: true,
            spectrum: [level; fxsound_core::NUM_SPECTRUM_BARS],
            sample_rate: 48_000,
            ..Meters::default()
        };
        engine.set_meters(OUT, playing(0.4));
        app.poll_audio();
        assert!(app.meters_moved());
        app.poll_audio();
        assert!(!app.meters_moved(), "the same meters again");
        engine.set_meters(OUT, playing(0.5));
        app.poll_audio();
        assert!(app.meters_moved());
        // The microphone off screen is not what the visualizer shows.
        engine.set_meters(IN, playing(0.9));
        app.poll_audio();
        assert!(!app.meters_moved());
    }

    #[test]
    fn a_controller_with_no_engine_has_no_notifications_to_wait_on_and_no_meters_moving() {
        let mut app = headless();
        assert!(app.audio_notifications().is_none());
        app.poll_audio();
        assert!(!app.meters_moved());
    }
}
