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
    messages::{AudioToUi, DspEvent, DspParams, InputDspParams, UiToAudio},
    scale,
};
use fxsound_preset::PresetStore;
use fxsound_ui::{
    AssetCache, Palette, UiAction, UiState,
    dialogs::{ExportState, ImportState, ImportSummary, OverwriteChoice, PresetsAction},
    state::PresetEntry,
};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::notify::{Message, Notifier};
use fxsound_core::i18n::{self, tr, tr_args};
use fxsound_ui::dialogs::settings::{DevicePriority, SettingsState};

/// The characters `PresetNameInputFilter` strips from a typed preset name
/// (`FxPresetNameEditor.cpp:6-33`): the Windows reserved-filename set, kept on Linux so a preset
/// saved here can be copied to a Windows FxSound unchanged (`docs/spec/03-controls.md` §11.2).
pub const FORBIDDEN_PRESET_NAME_CHARS: &str = "<>:\"/\\|?*";

/// `setInputRestrictions(64)` (`FxPresetNameEditor.cpp:52`, `FxMainWindow.cpp:58`).
pub const MAX_PRESET_NAME_CHARS: usize = 64;

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

/// What the window shows for one lane, kept here while the other lane is the one being edited.
///
/// A lane's controls live in [`UiState`] while that lane is the edit direction. Switching the edit
/// direction parks them here and puts the other lane's back, so a look at the speakers' list never
/// costs an unsaved equalizer move on the microphone, and never re-applies a preset under a chain
/// that is running. The published snapshots (`params`, `input_params`) are what the engine runs;
/// this is only what the window shows. The 0.4.0 controller proper keeps a `PerDirection` of these
/// and drops the parking.
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
    /// The shipped voice presets, read once at start-up.
    ///
    /// A separate list from `presets`, not a second source for the same one: a `.fac` and a voice
    /// preset describe different chains, and the only thing they share is that exactly one of them
    /// is selected at a time — whichever direction is live.
    input_presets: Vec<fxsound_preset::input::InputPreset>,
    /// Whether a device list has ever arrived from the audio thread.
    ///
    /// The control socket answers as soon as the GUI thread is up, which is before PipeWire has
    /// finished enumerating. Until this is true, "no device is called that" and "no device list
    /// yet" are the same observation, and they call for opposite answers.
    devices_seen: bool,
    /// The last thing the audio thread said about itself, for `--status`.
    audio_status: fxsound_core::AudioStatus,
    /// A `--output` that arrived before the list did, waiting for it.
    pending_device: Option<String>,
    /// Persisted settings, saved when they change rather than on a timer.
    settings: Settings,
    /// Factory and user presets.
    presets: PresetStore,
    /// The preset as loaded, so "undo changes" has something to go back to.
    loaded_preset: Option<Preset>,
    /// The audio engine, or `None` when PipeWire could not be reached.
    engine: Option<EngineHandle>,
    /// Rasterised artwork.
    pub assets: AssetCache,
    /// Whether the settings file needs writing.
    settings_dirty: bool,
    /// `false` in tests, so a test run can never write over the user's real settings file.
    persist: bool,
    /// Where Export Presets writes. A field rather than a constant so the tests can point it at
    /// a scratch directory instead of the user's documents.
    export_dir: PathBuf,
    /// The device selection last handed to the engine, so the saved choice goes out once per
    /// appearance of that device rather than on every device list (see
    /// [`saved_device_to_announce`]).
    announced_device: Option<(String, DeviceDirection)>,
    /// The lane the user last picked a device for this session, which is the lane the single-lane
    /// engine serves. Kept apart from the edit direction, which only says what the window shows
    /// (see [`lane_to_announce`]). Not saved: after a restart the lanes' enabled flags decide.
    last_picked_lane: Option<DeviceDirection>,
    /// The desktop-notification worker (`docs/spec/06-dialogs.md` §6.6).
    notifier: Notifier,
    /// Toasts are held back until construction is over: adopting the saved preset at start-up
    /// is not something to announce.
    notifications_armed: bool,
    /// The "FxSound in system tray" tip is shown once per process (`FxController.cpp:920-926`).
    tray_tip_shown: bool,
    /// The controls of the lane that is not being edited, once it has been edited this session.
    parked: Option<(DeviceDirection, LaneControls)>,
}

impl App {
    /// Build the application state, load the settings and presets, and adopt the saved presets.
    ///
    /// A missing sound server is not fatal: the UI comes up, says so, and keeps working, which is
    /// far more useful than refusing to start.
    #[must_use]
    pub fn new(engine: Option<EngineHandle>) -> Self {
        let settings = Settings::load();
        let mut presets = PresetStore::with_default_dirs();
        presets.rescan();

        let notifier = Notifier::new(settings.hide_notifications);
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
            input_presets: Vec::new(),
            devices_seen: false,
            audio_status: fxsound_core::AudioStatus::default(),
            pending_device: None,
            settings,
            presets,
            loaded_preset: None,
            engine,
            assets: AssetCache::new(),
            settings_dirty: false,
            persist: true,
            export_dir: default_export_dir(),
            announced_device: None,
            last_picked_lane: None,
            notifier,
            notifications_armed: false,
            tray_tip_shown: false,
            parked: None,
        };

        // What the settings file asks of the audio thread before it does anything else. See
        // [`startup_messages`].
        if let Some(engine) = &app.engine {
            for message in startup_messages(&app.settings) {
                engine.send(message);
            }
        }

        app.input_presets = fxsound_preset::input::InputPreset::load_shipped();
        app.adopt_saved_presets();
        app.notifications_armed = true;
        app
    }

    /// Bring **both** lanes up on the presets the settings file remembers for them.
    ///
    /// Both, not only the one the window was last editing: which lane the engine serves is not the
    /// edit direction (see [`lane_to_announce`]), so a restart that loaded only the edit
    /// direction's preset left the speakers flat — every effect at zero — for anyone whose window
    /// was last on the microphone, and a microphone handed to the engine by switching the speakers
    /// off ran unvoiced. The lane off screen goes first, loaded exactly the way a first visit loads
    /// it, and is then parked; the edit direction is entered the same way, so each lane's controls
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

    /// Pull everything the audio thread has published. Call once per frame, before rendering.
    pub fn poll_audio(&mut self) {
        // A notice is up for four seconds (0.4.0 design, §11). The views only draw it; this is
        // the one place that times it, engine or no engine.
        self.state.expire_notification(Instant::now());

        let Some(engine) = self.engine.as_mut() else {
            return;
        };

        let meters = engine.meters();
        self.state.spectrum = meters.spectrum;
        self.state.audio_active = meters.active;
        // The rate is the device's, and the interface needs it for one thing: an equalizer band
        // centred at or above Nyquist cannot be built, and a fader that does nothing has to say
        // so. A 16 kHz Bluetooth capture takes the top two bands of the standard ladder with it.
        self.state.sample_rate = meters.sample_rate;
        self.state.gate_reduction_db = meters.gate_reduction_db;
        self.state.deesser_running = meters.deesser_running;
        self.state.denoise_running = meters.denoiser_running;
        self.state.voice_probability = meters.voice_probability;
        self.state.compressor_reduction_db = meters.compressor_reduction_db;
        self.state.deesser_reduction_db = meters.deesser_reduction_db;
        // Copied as it is: `0.0` is what no measurement looks like (`Meters::default()`, and the
        // running minimum before it has come down), and the strip reads it as a dash rather than
        // as a room at full scale.
        self.state.noise_floor_db = meters.noise_floor_db;
        self.state.denoise_reduction_db = meters.denoise_reduction_db;
        self.state.dereverb_reduction_db = meters.dereverb_reduction_db;
        self.state.deesser_hz = meters.deesser_hz;

        let mut device_lists = Vec::new();
        while let Some(message) = engine.try_recv() {
            match message {
                // Taken after the loop, where the engine is no longer borrowed and the whole
                // controller is free to look at the list.
                AudioToUi::Devices(devices) => device_lists.push(devices),
                // One status per lane in 0.4.0; until the second lane exists in this crate, the
                // engine sends one and this keeps it for `--status`, and its lane's activity.
                AudioToUi::Status { direction, status } => {
                    match direction {
                        DeviceDirection::Output => self.state.output_active = status.processing,
                        DeviceDirection::Input => self.state.input_active = status.processing,
                    }
                    self.audio_status = status;
                }
                AudioToUi::Disconnected { reason } => {
                    self.state
                        .notify(format!("{} {reason}", tr("Audio disconnected:")));
                    // `"Output Disconnected"` (`FxController.cpp:1170`). A field access, not
                    // `Self::notify`, because the engine handle is still borrowed here.
                    if self.notifications_armed {
                        let _ = self.notifier.notify(Message::output_disconnected());
                    }
                }
                AudioToUi::Error { message, .. } => {
                    self.state.notify(message);
                }
                AudioToUi::EchoCancel { running, detail } => {
                    self.state.echo_cancel_running = running;
                    self.echo_cancel_detail = detail;
                }
                // What a lane is really attached to. The selection shown is still the user's
                // choice from the settings: the single-lane engine this crate runs against today
                // reports its one lane moving between directions, which is not a lane going away.
                // The two-lane controller reads this instead.
                AudioToUi::Attached { .. } => {}
                AudioToUi::RememberedDefault {
                    direction,
                    node_name,
                } => {
                    // Straight to the settings file. The audio thread's own copy dies with the
                    // process, and the three ways a process dies without warning — SIGKILL, the
                    // OOM killer, a power cut — are exactly the ones that leave the session
                    // default naming a node that is gone.
                    if self.settings.remembered_default(direction) != node_name {
                        self.settings.set_remembered_default(direction, &node_name);
                        self.settings_dirty = true;
                    }
                }
            }
        }

        // In arrival order, so a device that vanished in one list and came back in the next is
        // announced afresh.
        for devices in device_lists {
            if let Some(message) = self.adopt_device_list(devices)
                && let Some(engine) = &self.engine
            {
                engine.send(message);
            }
        }

        // Outside the loop, where the engine is no longer borrowed: selecting a device calls back
        // into the whole controller.
        if self.devices_seen && self.pending_device.is_some() {
            self.apply_pending_device();
        }
    }

    /// Take a device list from the engine, and say what, if anything, the engine has to be told.
    ///
    /// Each lane keeps the user's choice across a rescan when the device is still there, and
    /// otherwise falls back to whatever the server calls the default. A detached lane shows
    /// nothing: that is what `Off` means.
    fn adopt_device_list(&mut self, devices: Vec<AudioDevice>) -> Option<UiToAudio> {
        self.devices_seen = true;
        for direction in [DeviceDirection::Output, DeviceDirection::Input] {
            self.state.set_selection(
                direction,
                lane_selection(&self.settings, &devices, direction),
            );
        }
        self.state.devices = devices;
        self.saved_device_for_engine()
    }

    /// The engine starts in the output direction and picks by the device rules alone; the saved
    /// choice — and with it a saved microphone — reaches it from here, the first time that device
    /// is listed, for the lane [`lane_to_announce`] names.
    fn saved_device_for_engine(&mut self) -> Option<UiToAudio> {
        let direction = lane_to_announce(&self.settings, self.last_picked_lane)?;
        saved_device_to_announce(
            &self.settings,
            direction,
            &self.state.devices,
            &mut self.announced_device,
        )
    }

    /// Act on everything the views reported this frame.
    pub fn handle(&mut self, actions: &[UiAction]) {
        for action in actions {
            self.handle_one(action.clone());
        }
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
                self.sync_params_from_state();
                // Coming back from a bypass, the filters still hold whatever was in them when the
                // power went off, and `Chain::set_power` clears only the five effects — never the
                // equalizer or the leveller. That also makes the power button the one recovery a
                // user with broken-sounding audio will reach for first, so it has to be the one
                // that actually clears the history. Both the tray and `--power` route here.
                if self.state.power
                    && let Some(engine) = &self.engine
                {
                    engine.send_event(DspEvent::ResetFilterState);
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
            }

            UiAction::SelectPreset(index) => self.select_preset(index),
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
                if self.state.direction == DeviceDirection::Output {
                    self.settings.master_gain = self.state.master_gain_db;
                    self.settings_dirty = true;
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
        }
    }

    /// Rebuild the list the picker shows, from whichever chain is live.
    ///
    /// The two sets are never merged. A music preset on a microphone is wrong by construction, and
    /// a list mixing both would make picking the wrong one a normal thing to do.
    fn refresh_preset_list(&mut self) {
        self.state.presets = match self.state.direction {
            DeviceDirection::Output => self
                .presets
                .entries()
                .iter()
                .map(|entry| PresetEntry {
                    name: entry.name.clone(),
                    factory: entry.source == fxsound_preset::PresetSource::Factory,
                    modified: entry.modified,
                })
                .collect(),
            // Voice presets are read-only for now: they ship with the application, nothing writes
            // one, and `modified` is therefore always false. Saving over one is the next piece of
            // work, and until it exists the interface should not imply it is possible.
            DeviceDirection::Input => self
                .input_presets
                .iter()
                .map(|preset| PresetEntry {
                    name: preset.name.clone(),
                    factory: true,
                    modified: false,
                })
                .collect(),
        };
    }

    /// Put a voice preset's settings into the interface and publish them.
    ///
    /// The four stage switches live in [`UiState`] rather than in the mapping precisely so that
    /// this can move them: without it a preset's gate is a number nothing reads.
    fn apply_input_preset(&mut self, preset: &fxsound_preset::input::InputPreset) {
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

    fn select_preset(&mut self, index: usize) {
        let Some(entry) = self.state.presets.get(index) else {
            return;
        };
        let name = entry.name.clone();

        // A microphone's presets are a different set in a different format, and they are read-only:
        // none of the autosave, overwrite or modified-marker machinery below applies to one yet.
        if self.state.direction == DeviceDirection::Input {
            let Some(preset) = self
                .input_presets
                .iter()
                .find(|preset| preset.name == name)
                .cloned()
            else {
                return;
            };
            self.apply_input_preset(&preset);
            self.state.selected_preset = Some(index);
            self.notify(Message::preset_selected(&name));
            // Recorded against the lane rather than the edit direction, which start-up has not
            // entered yet when it loads the lane off screen.
            self.settings
                .set_preset_for_direction(DeviceDirection::Input, &name);
            self.settings_dirty = true;
            if let Some(engine) = &self.engine {
                engine.send_event(DspEvent::ResetFilterState);
            }
            return;
        }

        // Switching away from unsaved edits stashes them, exactly as the original does
        // (`FxController.cpp:1061-1065`), so nothing the user did is silently lost.
        if let Some(current) = self.state.preset()
            && current.modified
            && current.name != name
            && let Some(preset) = self.current_preset_snapshot()
            && let Err(err) = self.presets.autosave(&preset)
        {
            log::warn!("could not autosave {}: {err}", preset.name);
        }

        match self.presets.load(&name) {
            Ok((preset, from_autosave)) => {
                self.apply_preset(&preset);
                self.loaded_preset = Some(preset);
                self.state.selected_preset = Some(index);
                if let Some(entry) = self.state.presets.get_mut(index) {
                    entry.modified = from_autosave;
                }
                // `"Preset: "` + name on every change (`FxController.cpp:1101`).
                self.notify(Message::preset_selected(&name));
                // Record it against whatever is playing, so plugging the headphones back in
                // brings this preset with them. `DeviceConfig` and its two accessors were written
                // and tested for exactly this and then never called by anything.
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
                        &name,
                        &form_factor,
                        direction,
                    );
                }
                self.settings
                    .set_preset_for_direction(DeviceDirection::Output, &name);
                self.settings_dirty = true;
                // A new band layout means the old filter history is meaningless.
                if let Some(engine) = &self.engine {
                    engine.send_event(DspEvent::ResetFilterState);
                }
            }
            Err(err) => {
                log::warn!("could not load preset {name}: {err}");
                self.state
                    .notify(tr_args("Could not load %s", &[name.as_str()]));
            }
        }
    }

    fn apply_preset(&mut self, preset: &Preset) {
        for effect in Effect::ALL {
            self.state.effects[effect as usize] =
                scale::value_to_slider_for(effect, preset.effect(effect));
        }
        self.state.eq_bands = preset.eq_bands.clone();
        self.state.eq_on = preset.eq_on;
        self.sync_params_from_state();
    }

    /// The current UI state expressed as a preset, ready to save.
    fn current_preset_snapshot(&self) -> Option<Preset> {
        let name = self.state.preset()?.name.clone();
        let mut preset = self.loaded_preset.clone().unwrap_or_default();
        preset.name = name;
        for effect in Effect::ALL {
            preset.set_effect(
                effect,
                scale::slider_to_value_for(effect, self.state.effects[effect as usize]),
            );
        }
        preset.eq_bands = self.state.eq_bands.clone();
        preset.eq_on = self.state.eq_on;
        Some(preset)
    }

    fn mark_preset_modified(&mut self) {
        if let Some(index) = self.state.selected_preset
            && let Some(entry) = self.state.presets.get_mut(index)
        {
            entry.modified = true;
        }
    }

    fn save_preset(&mut self, new_name: Option<String>) {
        let Some(mut preset) = self.current_preset_snapshot() else {
            return;
        };
        let is_new = new_name.is_some();
        let name = new_name.unwrap_or_else(|| preset.name.clone());
        preset.name = name.clone();

        match self.presets.save_as(&preset, &name) {
            Ok(_) => {
                self.loaded_preset = Some(preset);
                self.refresh_preset_list();
                self.state.selected_preset = self.presets.index_of(&name);
                self.settings.set_selected_preset(&name);
                self.settings_dirty = true;
                // `FxController.cpp:1221` / `:1234`, the same text on the desktop and in the strip.
                let message = if is_new {
                    Message::preset_saved(&name)
                } else {
                    Message::preset_overwritten(&name)
                };
                self.state.notify(message.body.clone());
                self.notify(message);
            }
            Err(err) => {
                log::warn!("could not save preset {name}: {err}");
                self.state
                    .notify(tr_args("Could not save %s", &[name.as_str()]));
            }
        }
    }

    fn undo_preset_changes(&mut self) {
        let Some(index) = self.state.selected_preset else {
            return;
        };
        let Some(name) = self.state.presets.get(index).map(|p| p.name.clone()) else {
            return;
        };
        self.presets.clear_autosave(&name);
        self.select_preset(index);
    }

    fn delete_preset(&mut self) {
        let Some(index) = self.state.selected_preset else {
            return;
        };
        let Some(entry) = self.state.presets.get(index) else {
            return;
        };
        if entry.factory {
            self.state.notify(tr("Factory presets cannot be deleted"));
            return;
        }
        let name = entry.name.clone();
        match self.presets.delete(&name) {
            Ok(()) => {
                self.refresh_preset_list();
                let next = index.min(self.state.presets.len().saturating_sub(1));
                if self.state.presets.is_empty() {
                    self.state.selected_preset = None;
                } else {
                    self.select_preset(next);
                }
                let message = Message::preset_deleted(&name);
                self.state.notify(message.body.clone());
                self.notify(message);
            }
            Err(err) => {
                log::warn!("could not delete preset {name}: {err}");
                self.state
                    .notify(tr_args("Could not delete %s", &[name.as_str()]));
            }
        }
    }

    fn set_band_count(&mut self, count: usize) {
        let count = count.clamp(1, fxsound_core::eq::MAX_BANDS);
        if count == self.state.eq_bands.len() {
            return;
        }
        // Take the engine's own ladder for the new count so the frequencies stay sensible; the
        // gains start flat, as they do in the original when the band count changes.
        let centers = fxsound_dsp::eq::band_table(count).map_or_else(
            || {
                let mut eq = fxsound_dsp::GraphicEq::new();
                eq.set_num_bands(count);
                eq.center_frequencies().to_vec()
            },
            |(table, _, _)| table.to_vec(),
        );
        self.state.eq_bands = centers.into_iter().map(|hz| EqBand::new(hz, 0.0)).collect();
        self.settings.num_bands = count as u32;
        self.settings_dirty = true;
        self.mark_preset_modified();
        self.sync_params_from_state();
        if let Some(engine) = &self.engine {
            engine.send_event(DspEvent::ResetFilterState);
        }
    }

    fn restore_defaults(&mut self) {
        for band in &mut self.state.eq_bands {
            band.boost_db = 0.0;
        }
        self.state.filter_q = 1.0;
        self.state.master_gain_db = 0.0;
        self.state.balance_db = 0.0;
        self.state.volume_leveling = 0.0;
        if self.state.direction == DeviceDirection::Output {
            self.settings.filter_q = 1.0;
            self.settings.master_gain = 0.0;
            self.settings.balance = 0.0;
            self.settings.volume_leveling = 0.0;
            self.settings_dirty = true;
        }
        self.mark_preset_modified();
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
    }

    /// Attach `lane` to device `index` — the one path the window, the tray and the command line
    /// all take.
    ///
    /// The other lane is left exactly as it is: picking a microphone no longer means the speakers
    /// went away. Picking a device also makes its lane the edit direction, since that is the chain
    /// the user has just shown an interest in.
    fn select_device(&mut self, index: usize, lane: DeviceDirection) {
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

        self.state.set_selection(direction, Some(index));
        self.settings.set_device_name(direction, &name);
        self.settings.set_lane_enabled(direction, true);
        self.settings_dirty = true;
        self.last_picked_lane = Some(direction);
        // The interface follows the device immediately rather than waiting for the next device
        // list: picking a microphone is exactly the moment the five effect sliders stop meaning
        // anything, and a frame of them still looking live is a frame of lying.
        self.set_edit_direction(direction);
        if let Some(engine) = &self.engine {
            engine.send(UiToAudio::SelectDevice {
                node_name: name.clone(),
                direction,
            });
            self.announced_device = Some((name.clone(), direction));
        }

        // A device the user has used before brings its preset back with it. Only a *remembered*
        // one does: the first time something is plugged in, whatever is selected stays selected,
        // because guessing then would be changing the sound on no evidence at all. (Moving to the
        // other lane already brought back what that lane last had, in `set_edit_direction`.)
        if let Some(preset) = remembered
            && self
                .state
                .preset()
                .is_none_or(|current| current.name != preset)
            && let Some(at) = self.state.presets.iter().position(|e| e.name == preset)
        {
            self.select_preset(at);
        }

        // `"Output: "` + name, and the preset in use with it (`FxController.cpp:1150`).
        let preset = self.state.preset().map(|p| p.name.clone());
        self.notify(Message::output_selected(&description, preset.as_deref()));
    }

    /// Detach a lane: hand its default back, stop processing it, remember that it is off.
    ///
    /// The engine this crate runs against today has one lane and ignores `DetachLane`. So that
    /// switching off the lane it serves still does something, it is then sent the other lane's
    /// saved device when that lane is enabled — which is where the two-lane engine would be
    /// running anyway. With both lanes off it keeps what it had until the two-lane engine lands.
    fn detach_lane(&mut self, direction: DeviceDirection) {
        self.state.set_selection(direction, None);
        match direction {
            DeviceDirection::Output => self.state.output_active = false,
            DeviceDirection::Input => self.state.input_active = false,
        }
        self.settings.set_lane_enabled(direction, false);
        self.settings_dirty = true;
        if let Some(engine) = &self.engine {
            engine.send(UiToAudio::DetachLane(direction));
        }
        // Picking the same device again has to reach the engine again.
        if self
            .announced_device
            .as_ref()
            .is_some_and(|(_, announced)| *announced == direction)
        {
            self.announced_device = None;
        }
        if self.last_picked_lane == Some(direction) {
            self.last_picked_lane = None;
        }
        if self.engine.is_some()
            && let Some(message) = self.saved_device_for_engine()
            && let Some(engine) = &self.engine
        {
            engine.send(message);
        }
    }

    /// Make `direction` the lane the window edits. Returns whether it changed.
    ///
    /// Nothing the engine runs changes: this swaps what the window *shows*. The lane being left is
    /// parked as it is, unsaved edits and all; the lane being entered comes back as it was left, or
    /// — the first time this session — with the preset the settings file remembers for it, loaded
    /// the way start-up loads one, without a toast.
    fn set_edit_direction(&mut self, direction: DeviceDirection) -> bool {
        if direction == self.state.direction {
            if self.settings.device_direction != direction {
                self.settings.set_edit_direction(direction);
                self.settings_dirty = true;
            }
            return false;
        }
        let leaving = (self.state.direction, LaneControls::take(&self.state));
        self.state.direction = direction;
        self.settings.set_edit_direction(direction);
        self.settings_dirty = true;

        match self.parked.take() {
            Some((parked, controls)) if parked == direction => {
                let selected = controls
                    .selected_preset
                    .and_then(|i| controls.presets.get(i))
                    .map(|p| (p.name.clone(), p.modified));
                controls.put(&mut self.state);
                // The list may have changed while it was parked — an import, a rename — so it is
                // rebuilt, and the selection found again by name with its unsaved-changes marker.
                self.refresh_preset_list();
                self.state.selected_preset = None;
                if let Some((name, modified)) = selected
                    && let Some(at) = self.state.presets.iter().position(|p| p.name == name)
                {
                    self.state.selected_preset = Some(at);
                    self.state.presets[at].modified |= modified;
                }
                self.sync_params_from_state();
            }
            _ => self.enter_lane_for_the_first_time(direction),
        }
        self.parked = Some(leaving);
        true
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
    }

    /// The same controls, mapped onto the microphone chain.
    ///
    /// Both snapshots are published every time, whichever direction the engine is in: they are
    /// state, so the audio thread always reads a current one and a direction switch needs no
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

    /// Act on a `--output` that arrived before the device list did.
    ///
    /// Taken rather than retried: a name that is not in the list *now that there is one* is a name
    /// that is not a device, and holding it for the next list would mean a typo at login quietly
    /// changing the device half a minute later when something unrelated is plugged in.
    fn apply_pending_device(&mut self) {
        let Some(wanted) = self.pending_device.take() else {
            return;
        };
        let index = self
            .state
            .devices
            .iter()
            .position(|d| d.name == wanted)
            .or_else(|| {
                self.state
                    .devices
                    .iter()
                    .position(|d| d.description == wanted)
            });
        match index {
            Some(index) => self.handle(&[UiAction::SelectDevice(index)]),
            // The invoking process has long since exited, so there is nobody left to return a
            // status to. The log is the only place this can be said.
            None => log::warn!("no audio device is called {wanted:?}; --output did nothing"),
        }
    }

    /// What the audio thread last said about itself — the negotiated format, and how the ring
    /// between the two nodes is coping.
    #[must_use]
    pub const fn audio_status(&self) -> &fxsound_core::AudioStatus {
        &self.audio_status
    }

    /// Whether a device list has ever arrived.
    #[must_use]
    pub const fn has_seen_devices(&self) -> bool {
        self.devices_seen
    }

    /// Select this device as soon as a device list exists.
    ///
    /// Used by `--output` when it runs before enumeration has finished, which is what anything
    /// started at login does. One pending name, not a queue: a second `--output` supersedes the
    /// first exactly as a second one would if both had arrived after the list.
    pub fn select_device_when_listed(&mut self, name: &str) {
        self.pending_device = Some(name.to_owned());
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
        Self {
            state: UiState::default(),
            params: DspParams::default(),
            input_params: unvoiced_input_params(),
            input_voicing: PresetVoicing::of(&unvoiced_input_params()),
            echo_cancel_detail: String::new(),
            input_chain: fxsound_preset::input::DEFAULT_CHAIN.to_owned(),
            input_presets: Vec::new(),
            devices_seen: false,
            audio_status: fxsound_core::AudioStatus::default(),
            pending_device: None,
            settings: Settings::default(),
            presets: PresetStore::with_dirs(
                Vec::new(),
                std::env::temp_dir().join("fxsound-app-test"),
            ),
            loaded_preset: None,
            engine: None,
            assets: AssetCache::new(),
            settings_dirty: false,
            persist: false,
            export_dir: std::env::temp_dir().join("fxsound-app-test-export"),
            announced_device: None,
            last_picked_lane: None,
            notifier: Notifier::new(true),
            notifications_armed: true,
            tray_tip_shown: false,
            parked: None,
        }
    }

    /// Persist settings and stash unsaved preset edits. Called on the way out.
    pub fn shutdown(&mut self) {
        if let Some(preset) = self.current_preset_snapshot()
            && self.state.preset().is_some_and(|p| p.modified)
            && let Err(err) = self.presets.autosave(&preset)
        {
            log::warn!("could not autosave on exit: {err}");
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

    /// A fresh mirror of the model for the tray to draw its menu and tooltip from.
    #[must_use]
    pub fn tray_state(&self) -> crate::tray::TrayState {
        crate::tray::TrayState {
            power: self.state.power,
            processing: self.state.audio_active,
            power_enabled: true,
            theme: self.state.theme,
            always_on_top: self.settings.always_on_top,
            presets: self
                .state
                .presets
                .iter()
                .map(|p| crate::tray::TrayPreset {
                    name: p.name.clone(),
                    factory: p.factory,
                    modified: p.modified,
                })
                .collect(),
            selected_preset: self.state.selected_preset,
            devices: self
                .state
                .devices
                .iter()
                .map(|d| crate::tray::TrayDevice {
                    name: d.description.clone(),
                    // PipeWire sinks this port will render to are stereo or better; the field
                    // exists so the menu can grey out a mono device, as the original does.
                    channels: 2,
                    direction: d.direction,
                })
                .collect(),
            selected_device: self.state.selected_device(),
            language: i18n::current(),
            pixmaps: crate::tray::TrayPixmaps::default(),
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
            TrayCommand::SelectPreset(index) => self.handle(&[UiAction::SelectPreset(index)]),
            TrayCommand::SelectDevice(index) => self.handle(&[UiAction::SelectDevice(index)]),
            TrayCommand::SetTheme(theme) => {
                if theme != self.state.theme {
                    self.handle(&[UiAction::ToggleTheme]);
                }
            }
            TrayCommand::SetAlwaysOnTop(on) => {
                self.settings.always_on_top = on;
                self.settings_dirty = true;
                self.handle(&[]);
            }
            TrayCommand::OpenSettings => self.handle(&[UiAction::OpenSettings]),
            // Handled by the shell.
            TrayCommand::ToggleWindow | TrayCommand::Open | TrayCommand::Exit => {}
        }
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

    /// `FxController::renamePreset` — the menu's Rename Preset item.
    ///
    /// User presets only; a factory preset refuses with a notification, as deleting one does.
    /// Implemented as save-under-the-new-name then delete-the-old, which is what the original does
    /// through its preset list rather than a filesystem rename. The saved file is what gets
    /// renamed, not unsaved edits: the menu only offers Rename while the preset is unmodified
    /// (`docs/spec/03-controls.md` §8.5), and a caller that ignores that keeps its edits in the
    /// autosave under the *old* name, which `PresetStore::delete` then removes.
    pub fn rename_preset(&mut self, new_name: &str) {
        let new_name = new_name.trim();
        let Some(entry) = self.state.preset() else {
            return;
        };
        if entry.factory {
            self.state.notify(tr("Factory presets cannot be renamed"));
            return;
        }
        let old = entry.name.clone();
        if new_name.is_empty() || new_name == old {
            return;
        }
        if !self.is_preset_name_available(new_name) {
            self.state
                .notify(tr_args("A preset named %s already exists", &[new_name]));
            return;
        }

        let (mut preset, _) = match self.presets.load(&old) {
            Ok(loaded) => loaded,
            Err(err) => {
                log::warn!("could not load preset {old} to rename it: {err}");
                self.state
                    .notify(tr_args("Could not rename %s", &[old.as_str()]));
                return;
            }
        };
        preset.name = new_name.to_owned();
        if let Err(err) = self.presets.save_as(&preset, new_name) {
            log::warn!("could not save preset {new_name}: {err}");
            self.state
                .notify(tr_args("Could not rename %s", &[old.as_str()]));
            return;
        }
        if let Err(err) = self.presets.delete(&old) {
            // The copy under the new name exists, so the rename has happened; only the cleanup
            // failed. Say so and carry on with the new name selected.
            log::warn!("could not remove the old preset {old}: {err}");
        }

        self.loaded_preset = Some(preset);
        self.refresh_preset_list();
        self.state.selected_preset = self.presets.index_of(new_name);
        self.settings.set_selected_preset(new_name);
        self.settings_dirty = true;
        self.state
            .notify(tr_args("Renamed %s to %s", &[old.as_str(), new_name]));
        // Direct callers (the menu) do not go through `handle`, so flush here.
        self.handle(&[]);
    }

    /// `FxController::importPresets()` (`FxController.cpp:1419-1458`) over the non-recursive
    /// `*.fac` glob of `FxPresetImportDialog.cpp:255-278`.
    ///
    /// Returns `None` when the folder holds no preset files at all — the caller shows
    /// `"Preset files not found in the selected folder."` and leaves the window open. Otherwise
    /// every file whose name is already taken (case-insensitively) is skipped and the rest are
    /// copied into the user preset directory.
    pub fn import_presets(&mut self, folder: &Path) -> Option<ImportSummary> {
        let mut files: Vec<PathBuf> = match std::fs::read_dir(folder) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && path.extension().is_some_and(|ext| {
                            ext.eq_ignore_ascii_case(fxsound_ui::dialogs::presets::PRESET_EXTENSION)
                        })
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
            // `PresetStore::import` names the preset after the file, so that is the name to check.
            if !self.is_preset_name_available(&stem) {
                summary.skipped.push(stem);
                continue;
            }
            match self.presets.import(&path) {
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
                    self.state.notify(tr_args(
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

    /// The presets among `names` whose file already exists in the export directory.
    fn export_collisions(&self, names: &[String]) -> Vec<String> {
        names
            .iter()
            .filter(|name| self.export_dir.join(export_file_name(name)).exists())
            .cloned()
            .collect()
    }

    /// Write `names` into the export directory. Returns how many files were written, which is
    /// what `FxController::exportPresets()` reduces to a `bool`.
    fn export_presets(&mut self, names: &[String]) -> usize {
        if let Err(err) = std::fs::create_dir_all(&self.export_dir) {
            log::warn!("could not create {}: {err}", self.export_dir.display());
            self.state.notify(tr("Could not create the export folder"));
            return 0;
        }
        let mut written = 0;
        for name in names {
            match self.presets.export(name, &self.export_dir) {
                Ok(_) => written += 1,
                Err(err) => {
                    log::warn!("could not export {name}: {err}");
                    self.state
                        .notify(tr_args("Could not export %s", &[name.as_str()]));
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
        self.state.selected_preset = self.presets.index_of(&name);
        if modified
            && let Some(index) = self.state.selected_preset
            && let Some(entry) = self.state.presets.get_mut(index)
        {
            entry.modified = true;
        }
    }
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
/// - Echo cancellation, when it was left on. It is otherwise sent only when the checkbox is
///   toggled, so a saved `echo_cancel = true` came back as a pane and a strip saying it had been
///   asked for, while the engine was never told and `module-echo-cancel` never loaded. Off is the
///   engine's own starting state and is not repeated.
fn startup_messages(settings: &Settings) -> Vec<UiToAudio> {
    let mut messages = vec![UiToAudio::SeedRememberedDefaults {
        output: settings.remembered_default_output.clone(),
        input: settings.remembered_default_input.clone(),
    }];
    if settings.echo_cancel {
        messages.push(UiToAudio::SetEchoCancel(true));
    }
    messages
}

/// An equalizer as the window holds it, from a snapshot's two parallel arrays.
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

/// Which device a lane shows after a device list arrives: the one the settings name for it when it
/// is listed, else the server's default of that direction; nothing at all for a detached lane.
fn lane_selection(
    settings: &Settings,
    devices: &[AudioDevice],
    direction: DeviceDirection,
) -> Option<usize> {
    if !settings.lane_enabled(direction) {
        return None;
    }
    let wanted = settings.device_name(direction);
    devices
        .iter()
        .position(|d| d.name == wanted && d.direction == direction)
        .or_else(|| {
            devices
                .iter()
                .position(|d| d.is_default && d.direction == direction)
        })
}

/// Which lane the saved device choice is announced for, while the engine has one lane to give.
///
/// The lane the user last picked a device for this session, while it is still enabled; else the
/// output lane when it is enabled — the engine starts there, and running it is the Windows
/// behaviour; else the input lane when it is enabled; else none, and the engine's own device
/// rules are left alone.
///
/// Never the edit direction. That says which chain the window shows, and looking at the other
/// lane's list must not change what the engine runs (0.4.0 design, §1.1) — least of all attach a
/// lane the user switched off. The 0.3.0 meaning of `device_direction` ("the live lane") is not
/// read here either: the loader has already turned it into `input_enabled`, after which it cannot
/// be told from a 0.4.0 edit direction, so on this single-lane engine a user who left 0.3.0 on a
/// microphone comes back to the speakers until they pick the microphone again. The two-lane
/// engine attaches every enabled lane, and this choice goes away with it.
fn lane_to_announce(
    settings: &Settings,
    last_picked: Option<DeviceDirection>,
) -> Option<DeviceDirection> {
    last_picked
        .filter(|&direction| settings.lane_enabled(direction))
        .or_else(|| {
            [DeviceDirection::Output, DeviceDirection::Input]
                .into_iter()
                .find(|&direction| settings.lane_enabled(direction))
        })
}

/// The saved device of `direction`, when it is time to send it to the engine.
///
/// The port of `FxController::init` step 5 (`docs/spec/05-controller-model.md` §9.2): once the
/// device list is known, a device named by the saved `output_device_name` is adopted and
/// `setOutput` forces it. Here the engine starts in the output direction and runs the device rules
/// on its own, so the saved choice — `settings.device_name(direction)` — is handed to it the first
/// time that device is listed; rule 2 of `choose_device` yields to it
/// (`docs/spec/12-audio-io.md` §28.5), and a saved microphone brings the engine round to the
/// source direction. A detached lane is never announced.
///
/// `announced` is what was last sent. A device list that merely changed *around* the saved device
/// sends nothing again; a device that vanished and came back is announced afresh, which the
/// engine's own rules make a no-op when it is already attached to it. An empty saved name never
/// matches a node, so nothing is sent on a first run.
fn saved_device_to_announce(
    settings: &Settings,
    direction: DeviceDirection,
    devices: &[AudioDevice],
    announced: &mut Option<(String, DeviceDirection)>,
) -> Option<UiToAudio> {
    let forget = |announced: &mut Option<(String, DeviceDirection)>| {
        if announced.as_ref().is_some_and(|(_, dir)| *dir == direction) {
            *announced = None;
        }
    };
    if !settings.lane_enabled(direction) {
        forget(announced);
        return None;
    }
    let wanted = settings.device_name(direction);
    if !devices
        .iter()
        .any(|d| d.name == wanted && d.direction == direction)
    {
        forget(announced);
        return None;
    }
    if announced
        .as_ref()
        .is_some_and(|(name, dir)| name == wanted && *dir == direction)
    {
        return None;
    }
    *announced = Some((wanted.to_owned(), direction));
    Some(UiToAudio::SelectDevice {
        node_name: wanted.to_owned(),
        direction,
    })
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

/// The file `PresetStore::export` writes `name` to, so a collision can be detected before the
/// overwrite prompt. Mirrors the store's private `sanitise` (`fxsound-preset/src/store.rs:311`):
/// only the path separators and NUL are replaced.
fn export_file_name(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | '\0') {
                '_'
            } else {
                c
            }
        })
        .collect();
    format!("{stem}.{}", fxsound_ui::dialogs::presets::PRESET_EXTENSION)
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
        state.presets = self.state.presets.iter().map(|p| p.name.clone()).collect();
        // The reset button is enabled iff there is something to lose (`FxSettingsDialog.cpp:210-220`).
        state.can_reset_presets = self.state.presets.iter().any(|p| !p.factory || p.modified);
        state.devices = self
            .settings
            .device_configs
            .iter()
            .map(|config| DevicePriority {
                id: config.device_id.clone(),
                name: config.device_name.clone(),
                preset: self
                    .state
                    .presets
                    .iter()
                    .position(|p| p.name == config.preset),
                connected: self
                    .state
                    .device()
                    .is_some_and(|d| d.name == config.device_id),
                present: self
                    .state
                    .devices
                    .iter()
                    .any(|d| d.name == config.device_id),
            })
            .collect();
        self.refresh_settings_state(&mut state);
        state
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
            A::SetLanguage(choice) => {
                // Live: every view asks `i18n::tr` on every frame, so the swap is visible on the
                // next one. The PipeWire node descriptions keep the language they were created
                // in — rebuilding both nodes for a caption would drop the audio for a moment.
                self.settings.choose_language(choice.as_deref());
                i18n::set_language(self.settings.effective_language());
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
                        self.state
                            .notify(tr("Could not change the startup setting"));
                    }
                }
            }

            A::RemoveDevice(index) => {
                if *index < self.settings.device_configs.len() {
                    self.settings.device_configs.remove(*index);
                    state
                        .settings
                        .device_configs
                        .clone_from(&self.settings.device_configs);
                    self.persist_settings();
                }
            }
            A::MoveDeviceUp(index) => self.swap_device_config(state, *index, index.wrapping_sub(1)),
            A::MoveDeviceDown(index) => self.swap_device_config(state, *index, index + 1),
            A::SetDevicePreset { device, preset } => {
                let name = self.state.presets.get(*preset).map(|p| p.name.clone());
                if let (Some(config), Some(name)) =
                    (self.settings.device_configs.get_mut(*device), name)
                {
                    config.preset = name;
                    state
                        .settings
                        .device_configs
                        .clone_from(&self.settings.device_configs);
                    self.persist_settings();
                }
            }

            A::ResetPresets => {
                // Drop every autosave, so each preset reverts to what shipped or was last saved.
                for entry in self.state.presets.clone() {
                    self.presets.clear_autosave(&entry.name);
                }
                self.presets.rescan();
                self.refresh_preset_list();
                if let Some(index) = self.state.selected_preset {
                    self.select_preset(index);
                }
                let message = Message::presets_restored();
                self.state.notify(message.body.clone());
                self.notify(message);
            }

            A::OpenUrl(url) => {
                if let Err(err) = open_url(url) {
                    log::warn!("could not open {url}: {err}");
                    self.state.notify(tr("Could not open the link"));
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
            A::ShowHotkeyExample
            | A::ShowChangelog
            | A::OpenCalibration
            | A::SelectDeviceRow(_)
            | A::Close => {}
        }
    }

    fn swap_device_config(
        &mut self,
        state: &mut fxsound_ui::dialogs::settings::SettingsState,
        a: usize,
        b: usize,
    ) {
        let len = self.settings.device_configs.len();
        if a < len && b < len {
            self.settings.device_configs.swap(a, b);
            state
                .settings
                .device_configs
                .clone_from(&self.settings.device_configs);
            self.persist_settings();
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
    /// echo canceller since the pane opened, and whether there is still a microphone to calibrate.
    /// The host calls this every frame the pane is open; everything else in the pane's working
    /// copy changes only through [`App::handle_settings`].
    pub fn refresh_settings_state(&self, state: &mut SettingsState) {
        state.echo_cancel_running = self.state.echo_cancel_running;
        if state.echo_cancel_detail != self.echo_cancel_detail {
            state
                .echo_cancel_detail
                .clone_from(&self.echo_cancel_detail);
        }
        state.has_microphone = self.microphone_description().is_some();
    }

    /// The selected microphone's description, which the calibration wizard shows under its title,
    /// or `None` with no microphone selected.
    #[must_use]
    pub fn microphone_description(&self) -> Option<&str> {
        self.state
            .device_for(DeviceDirection::Input)
            .map(|device| device.description.as_str())
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

    #[test]
    fn restoring_defaults_flattens_the_curve_and_the_levels() {
        let mut app = with_presets(&["Jazz"]);
        app.handle(&[
            UiAction::SetBandGain(2, 9.0),
            UiAction::SetMasterGain(-6.0),
            UiAction::SetVolumeLeveling(3.0),
        ]);
        app.handle(&[UiAction::RestoreDefaults]);
        assert!(app.state.eq_bands.iter().all(|b| b.boost_db == 0.0));
        assert_eq!(app.state.master_gain_db, 0.0);
        assert_eq!(app.state.volume_leveling, 0.0);
        assert_eq!(app.state.filter_q, 1.0);
    }

    #[test]
    fn changing_the_band_count_rebuilds_the_curve_flat() {
        let mut app = with_presets(&["Jazz"]);
        app.handle(&[UiAction::SetBandGain(0, 8.0)]);
        app.handle(&[UiAction::SetBandCount(31)]);
        assert_eq!(app.state.eq_bands.len(), 31);
        assert!(app.state.eq_bands.iter().all(|b| b.boost_db == 0.0));
        // The 31-band ISO ladder starts at 20 Hz and ends at 20 kHz.
        assert_eq!(app.state.eq_bands[0].center_hz, 20.0);
        assert_eq!(app.state.eq_bands[30].center_hz, 20000.0);
        assert_eq!(app.params().num_bands, 31);
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

    /// Two voice presets, so a crossing has somewhere to land.
    fn with_voice_presets(app: &mut App) {
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
            },
            makeup_db: 0.0,
            ceiling_db: -3.0,
            ..InputPreset::default()
        };
        app.input_presets = vec![voice("Clean Voice", 80.0), voice("Flat", 75.0)];
    }

    #[test]
    fn crossing_to_a_microphone_swaps_the_whole_preset_list() {
        // The two sets are never merged: a music preset on a voice is wrong by construction, and a
        // list holding both would make picking the wrong one a normal thing to do.
        let mut app = app_with_two_presets("directions");
        with_voice_presets(&mut app);
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
        app.input_presets = vec![
            InputPreset {
                name: "Voice".to_owned(),
                ..InputPreset::default()
            },
            InputPreset {
                name: "Podcast".to_owned(),
                chain: "podcast".to_owned(),
                ..InputPreset::default()
            },
        ];
        app.refresh_preset_list();
        assert_eq!(app.input_chain(), DEFAULT_CHAIN);

        app.handle(&[UiAction::SelectPreset(1)]);
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
        app.handle(&[UiAction::SelectPreset(0)]);
        assert_eq!(app.input_chain(), DEFAULT_CHAIN);
    }

    #[test]
    fn a_voice_preset_moves_the_stages_the_interface_has_no_control_for() {
        // The whole reason the voice set is navigated by preset: the gate, the compressor and the
        // de-esser have no knobs anywhere, so if a preset does not move them nothing does.
        use fxsound_preset::input::{Compressor, Equalizer, Gate, InputPreset};
        let mut app = App::headless_for_tests();
        app.state.direction = DeviceDirection::Input;
        app.input_presets = vec![InputPreset {
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
            },
            makeup_db: 4.0,
            ceiling_db: -3.0,
            ..InputPreset::default()
        }];
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
        app.select_device_when_listed("typo");
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
    fn a_detached_lane_shows_nothing_after_a_rescan_and_an_enabled_one_its_device() {
        let devices = vec![
            device("alsa_output.speakers", DeviceDirection::Output, true),
            device("alsa_input.mic", DeviceDirection::Input, true),
            device("alsa_input.webcam", DeviceDirection::Input, false),
        ];
        let mut settings = Settings::default();
        settings.set_lane_enabled(DeviceDirection::Input, false);
        settings.set_device_name(DeviceDirection::Input, "alsa_input.webcam");
        assert_eq!(
            lane_selection(&settings, &devices, DeviceDirection::Output),
            Some(0),
            "no saved output: the server's default"
        );
        assert_eq!(
            lane_selection(&settings, &devices, DeviceDirection::Input),
            None,
            "a detached lane stays off"
        );
        settings.set_lane_enabled(DeviceDirection::Input, true);
        assert_eq!(
            lane_selection(&settings, &devices, DeviceDirection::Input),
            Some(2),
            "the saved microphone, not the default one"
        );
        settings.set_device_name(DeviceDirection::Input, "unplugged");
        assert_eq!(
            lane_selection(&settings, &devices, DeviceDirection::Input),
            Some(1),
            "a missing device falls back to the default of its own direction"
        );
        settings.set_lane_enabled(DeviceDirection::Output, false);
        assert_eq!(
            lane_selection(&settings, &devices, DeviceDirection::Output),
            None
        );
    }

    #[test]
    fn switching_the_edit_direction_swaps_the_preset_list_and_back() {
        let mut app = app_with_two_presets("edit-direction");
        with_voice_presets(&mut app);
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
        with_voice_presets(&mut app);
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
        with_voice_presets(&mut app);
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
        assert!(app.parked.is_none(), "nothing was parked");
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
        with_voice_presets(&mut app);
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
        // The engine serves the speakers whenever they are on, whatever the window last edited:
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
        // Switching the speakers off hands the single-lane engine the microphone, which has to be
        // running its own preset by then, not the unvoiced chain.
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
        settings.set_device_name(IN, "alsa_input.usb-fifine");
        settings.set_lane_enabled(IN, true);
        let without = vec![device("alsa_output.pci", DeviceDirection::Output, true)];
        let with = vec![
            device("alsa_output.pci", DeviceDirection::Output, true),
            device("alsa_input.usb-fifine", DeviceDirection::Input, false),
        ];
        let mut announced = None;

        // Not listed yet: nothing to say.
        assert_eq!(
            saved_device_to_announce(&settings, IN, &without, &mut announced),
            None
        );
        // Listed: announced exactly once…
        assert_eq!(
            saved_device_to_announce(&settings, IN, &with, &mut announced),
            Some(UiToAudio::SelectDevice {
                node_name: "alsa_input.usb-fifine".to_owned(),
                direction: DeviceDirection::Input,
            })
        );
        assert_eq!(
            saved_device_to_announce(&settings, IN, &with, &mut announced),
            None
        );
        // …and once more after it was unplugged and came back.
        assert_eq!(
            saved_device_to_announce(&settings, IN, &without, &mut announced),
            None
        );
        assert!(saved_device_to_announce(&settings, IN, &with, &mut announced).is_some());
    }

    #[test]
    fn a_saved_name_is_only_announced_in_its_own_direction() {
        let mut settings = Settings::default();
        settings.set_device_name(DeviceDirection::Input, "fifine");
        settings.set_lane_enabled(DeviceDirection::Input, true);
        let devices = vec![device("fifine", DeviceDirection::Output, true)];
        let mut announced = None;
        assert_eq!(
            saved_device_to_announce(&settings, DeviceDirection::Input, &devices, &mut announced),
            None
        );
        assert!(announced.is_none());
    }

    #[test]
    fn with_no_saved_device_nothing_is_announced() {
        let settings = Settings::default();
        let devices = vec![device("alsa_output.pci", DeviceDirection::Output, true)];
        let mut announced = None;
        assert_eq!(
            saved_device_to_announce(&settings, DeviceDirection::Output, &devices, &mut announced),
            None
        );
    }

    // ---- which lane the engine is told about -----------------------------------------------

    const OUT: DeviceDirection = DeviceDirection::Output;
    const IN: DeviceDirection = DeviceDirection::Input;

    fn select(name: &str, direction: DeviceDirection) -> Option<UiToAudio> {
        Some(UiToAudio::SelectDevice {
            node_name: name.to_owned(),
            direction,
        })
    }

    fn both_devices() -> Vec<AudioDevice> {
        vec![
            device("alsa_output.pci", OUT, true),
            device("alsa_input.usb-fifine", IN, false),
        ]
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
            app.adopt_device_list(both_devices()),
            select("alsa_output.pci", OUT)
        );
        // Opening the other lane's list to look at it is a SetEditDirection and nothing more…
        app.handle(&[UiAction::SetEditDirection(IN)]);
        assert_eq!(app.settings.device_direction, IN);
        // …so the rescan that follows has nothing to tell the engine.
        assert_eq!(app.adopt_device_list(both_devices()), None);
        assert_eq!(app.adopt_device_list(both_devices()), None);
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
            app.adopt_device_list(both_devices()),
            select("alsa_output.pci", OUT)
        );
        // With no speaker remembered either, the engine is left to its own device rules.
        let mut app = app_remembering(false, IN);
        app.settings.set_device_name(OUT, "");
        assert_eq!(app.adopt_device_list(both_devices()), None);
    }

    #[test]
    fn a_restart_with_both_lanes_on_announces_the_speakers_whatever_the_window_edited() {
        for edit in [OUT, IN] {
            let mut app = app_remembering(true, edit);
            assert_eq!(
                app.adopt_device_list(both_devices()),
                select("alsa_output.pci", OUT),
                "editing {edit:?}"
            );
        }
    }

    #[test]
    fn with_the_speakers_switched_off_the_enabled_microphone_is_announced() {
        let mut app = app_remembering(true, OUT);
        app.settings.set_lane_enabled(OUT, false);
        assert_eq!(
            app.adopt_device_list(both_devices()),
            select("alsa_input.usb-fifine", IN)
        );
    }

    #[test]
    fn with_both_lanes_off_nothing_is_announced() {
        let mut app = app_remembering(false, IN);
        app.settings.set_lane_enabled(OUT, false);
        assert_eq!(app.adopt_device_list(both_devices()), None);
    }

    #[test]
    fn the_lane_last_picked_stays_the_one_announced_while_the_window_looks_elsewhere() {
        let mut app = app_remembering(true, OUT);
        app.state.devices = both_devices();
        app.handle(&[UiAction::SelectDevice(1)]);
        assert_eq!(app.last_picked_lane, Some(IN));
        app.handle(&[UiAction::SetEditDirection(OUT)]);
        assert_eq!(app.settings.device_direction, OUT);
        // Headless, so the pick itself sent nothing; the list after it names the microphone, the
        // lane that was picked — never the speakers the window has moved to.
        assert_eq!(
            app.adopt_device_list(both_devices()),
            select("alsa_input.usb-fifine", IN)
        );
        assert_eq!(app.adopt_device_list(both_devices()), None);
    }

    #[test]
    fn a_device_list_that_changes_around_the_served_lane_sends_nothing_again() {
        let mut app = app_remembering(true, IN);
        assert!(app.adopt_device_list(both_devices()).is_some());
        let mut more = both_devices();
        more.push(device("bluez_output.headset", OUT, false));
        assert_eq!(app.adopt_device_list(more), None);
    }

    #[test]
    fn switching_off_the_microphone_the_engine_serves_hands_it_back_to_the_speakers() {
        let mut app = app_remembering(true, OUT);
        app.state.devices = both_devices();
        app.handle(&[UiAction::SelectDevice(1)]);
        // What a send would have recorded, since a headless app has no engine to send to.
        app.announced_device = Some(("alsa_input.usb-fifine".to_owned(), IN));

        app.handle(&[UiAction::DetachInput]);

        assert_eq!(app.last_picked_lane, None);
        assert!(!app.settings.lane_enabled(IN));
        assert_eq!(
            app.saved_device_for_engine(),
            select("alsa_output.pci", OUT)
        );
    }

    #[test]
    fn switching_off_a_lane_the_engine_does_not_serve_leaves_the_served_one_alone() {
        let mut app = app_remembering(true, IN);
        app.state.devices = both_devices();
        app.handle(&[UiAction::SelectDevice(0)]);
        app.announced_device = Some(("alsa_output.pci".to_owned(), OUT));

        app.handle(&[UiAction::DetachInput]);

        assert_eq!(app.last_picked_lane, Some(OUT));
        assert_eq!(app.saved_device_for_engine(), None);
    }

    #[test]
    fn a_detached_lane_is_forgotten_by_the_announcement_bookkeeping() {
        let mut settings = Settings::default();
        settings.set_device_name(IN, "alsa_input.usb-fifine");
        settings.set_lane_enabled(IN, false);
        let mut announced = Some(("alsa_input.usb-fifine".to_owned(), IN));
        assert_eq!(
            saved_device_to_announce(&settings, IN, &both_devices(), &mut announced),
            None
        );
        assert_eq!(announced, None);
        // …and a detached lane says nothing about the other lane's record.
        let mut announced = Some(("alsa_output.pci".to_owned(), OUT));
        assert_eq!(
            saved_device_to_announce(&settings, IN, &both_devices(), &mut announced),
            None
        );
        assert!(announced.is_some());
    }

    #[test]
    fn the_lane_to_announce_never_follows_the_edit_direction() {
        let mut settings = Settings::default();
        settings.set_lane_enabled(IN, false);
        for edit in [OUT, IN] {
            settings.set_edit_direction(edit);
            assert_eq!(lane_to_announce(&settings, None), Some(OUT));
            assert_eq!(
                lane_to_announce(&settings, Some(IN)),
                Some(OUT),
                "IN is off"
            );
        }
        settings.set_lane_enabled(IN, true);
        assert_eq!(lane_to_announce(&settings, Some(IN)), Some(IN));
        settings.set_lane_enabled(OUT, false);
        assert_eq!(lane_to_announce(&settings, None), Some(IN));
        assert_eq!(lane_to_announce(&settings, Some(OUT)), Some(IN));
        settings.set_lane_enabled(IN, false);
        assert_eq!(lane_to_announce(&settings, Some(IN)), None);
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
        assert!(app.announced_device.is_none());
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
    fn the_export_file_name_only_touches_path_separators() {
        assert_eq!(export_file_name("Rock & Roll"), "Rock & Roll.fac");
        assert_eq!(export_file_name("a/b\\c"), "a_b_c.fac");
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
        app.input_presets = vec![InputPreset {
            name: "Tuned".to_owned(),
            rnnoise: true,
            denoise: Some(Denoise {
                level: DenoiseLevel::Light,
                channels: DenoiseChannelMode::Linked,
                max_suppression_db: Some(9.0),
                ..Denoise::default()
            }),
            ..InputPreset::default()
        }];
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
        with_voice_presets(&mut app);
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

        app.state.selected_input = Some(1);
        assert!(app.settings_state().has_microphone);
        assert_eq!(app.microphone_description(), Some("alsa_input.mic"));

        // An index that names a speaker is not a microphone.
        app.state.selected_input = Some(0);
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
}
