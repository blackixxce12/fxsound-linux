//! Persisted application settings.
//!
//! The Windows build keeps these in a JUCE `PropertiesFile` XML at
//! `%APPDATA%\FxSound\FxSound.settings`. This port keeps the **same key names** in a TOML file at
//! `$XDG_CONFIG_HOME/fxsound/settings.toml` (default `~/.config/fxsound/settings.toml`) so the
//! schema stays greppable against the original C++ and a user's settings are recognisable.
//!
//! Three keys change meaning on Linux and say so in their doc comments: the hotkey commands
//! (a Wayland client cannot grab global shortcuts), `automatic_updates` (this fork is installed by
//! a package manager and never phones home) and the window position keys (Wayland gives a client
//! no way to place its own toplevel).
//!
//! A file that does not parse is moved aside as `settings.toml.bad` and the defaults are used —
//! never overwritten in place, so a hand edit that went wrong can still be read back by the
//! person who made it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    DeEsserMode, DenoiseChannelsOverride, DereverbLevel, DeviceDirection, NoiseSuppressionOverride,
};

/// Which of the two window layouts is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    /// The full window: visualizer, effect sliders and the graphic equalizer.
    #[default]
    Pro,
    /// The compact window: preset and output pickers only.
    Lite,
}

/// Light or dark palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

/// One remembered device and the preset the user last used with it.
///
/// Every field defaults, for the same reason the file as a whole does: an entry written by an
/// older version, or one a hand edit left short, must cost that entry's missing field and not
/// the whole settings file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DeviceConfig {
    /// On Windows an endpoint id; here the PipeWire `node.name`, which is stable across restarts.
    pub device_id: String,
    /// Which lane this memory belongs to. An entry without the key is an output entry: 0.3.0
    /// wrote only output devices here — its preset picker returned before the memory was
    /// written whenever the microphone was the live device — so there is no older microphone
    /// entry for the default to misread. The two directions are separate namespaces regardless,
    /// because a `.fac` name and a voice preset's name can collide.
    #[serde(default)]
    pub direction: DeviceDirection,
    /// Human-readable name (`node.description`).
    pub device_name: String,
    /// Preset last selected while this device was active.
    pub preset: String,
    /// `speaker`, `headphone`, `hdmi`, … as PipeWire spells them.
    ///
    /// Recorded so a device can be recognised by kind rather than only by name. Nothing chooses a
    /// preset from it yet — that would change which preset a user hears when they plug something
    /// in, which is a decision worth making deliberately rather than inheriting from a comment.
    pub device_form_factor: String,
}

/// What the microphone calibration wizard last measured and what it did about it.
///
/// Informational: the Settings pane shows it, and the preset the wizard wrote is a file of its
/// own. Nothing here reaches a filter design, so a corrupt record is dropped rather than clamped
/// — a floor of NaN is not a measurement that was merely too large.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CalibrationRecord {
    /// The floor measured during the silence phase, dBFS.
    pub noise_floor_db: f32,
    /// Mean RMS of the speech phase, dBFS.
    pub speech_rms_db: f32,
    /// Peak of the speech phase, dBFS.
    pub speech_peak_db: f32,
    /// Fraction of the loud phase's samples that clipped, `0.0..=1.0`.
    pub clipped_ratio: f32,
    /// When the wizard ran, as seconds since the Unix epoch.
    pub unix_time: u64,
    /// The voice preset the wizard wrote and selected.
    pub preset: String,
    /// `node.name` of the microphone it was run on.
    pub device: String,
}

impl CalibrationRecord {
    /// Whether every measurement in the record is a number.
    #[must_use]
    fn is_finite(&self) -> bool {
        [
            self.noise_floor_db,
            self.speech_rms_db,
            self.speech_peak_db,
            self.clipped_ratio,
        ]
        .iter()
        .all(|v| v.is_finite())
    }
}

/// Keyboard shortcuts.
///
/// On Windows these are Win32 `RegisterHotKey` codes. A Wayland client cannot register a global
/// shortcut, so here they are human-readable accelerator strings that the app only *displays*;
/// the binding itself lives in the compositor and reaches the running instance through the
/// control socket (see `packaging/hyprland.conf.example`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub cmd_on_off: String,
    pub cmd_open_close: String,
    pub cmd_next_preset: String,
    pub cmd_previous_preset: String,
    pub cmd_change_output: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        // The same chords the Windows build ships, decoded from its Win32 hotkey codes.
        Self {
            cmd_on_off: "Ctrl+Shift+Q".into(),
            cmd_open_close: "Ctrl+Shift+E".into(),
            cmd_next_preset: "Ctrl+Shift+A".into(),
            cmd_previous_preset: "Ctrl+Shift+Z".into(),
            cmd_change_output: "Ctrl+Shift+W".into(),
        }
    }
}

/// The whole settings file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // ---- audio -----------------------------------------------------------------------------
    /// Master power state, restored on launch.
    pub power: bool,
    /// Name of the preset to select on launch while FxSound sits in front of a playback device.
    ///
    /// A `settings.toml` written by 0.2.0 — which had one preset key because it had one chain —
    /// is read through the private `legacy_preset` below. On the next save the file is written
    /// under the new name only, so a 0.2.0 binary reading a 0.3.0 file falls back to its default
    /// preset: a cosmetic loss on a downgrade, not a data one, since the presets themselves are
    /// files of their own and are untouched.
    pub output_preset: String,
    /// Name of the preset to select while FxSound sits behind a microphone.
    ///
    /// Separate because the two chains are separate. A music preset on a voice is wrong by
    /// construction — reverberation, stereo widening and a bass lift are the opposite of what a
    /// voice wants — so carrying one across a direction switch would hand the user a sound nobody
    /// chose. The default is the voice set's own reference preset, not the output set's: a name
    /// that is in neither list means the picker lands on whatever happens to sort first.
    pub input_preset: String,
    /// The 0.2.0 spelling of [`Settings::output_preset`], folded in by [`Settings::sanitise`].
    ///
    /// `#[serde(alias)]` would have been one line, and was the first attempt. It is wrong here:
    /// serde rejects a document carrying both the alias and the real name as a **duplicate
    /// field**, and [`Settings::load`] turns any parse error into a full reset — so one stale
    /// `preset =` line left in a hand-edited file would have cost the user every setting they
    /// had. A field of its own cannot collide with anything, and is never written back.
    #[serde(rename = "preset", skip_serializing)]
    legacy_preset: Option<String>,
    /// `node.name` of the real output device FxSound renders to.
    pub output_device_name: String,
    /// `node.name` of the real capture device FxSound listens to.
    pub input_device_name: String,
    /// The **edit direction**: which lane the window's preset picker, equalizer, level controls
    /// and meters address. Linux only, and a GUI notion only — both lanes run regardless, and
    /// the engine never sees this. The key keeps its 0.3.0 name, under which it meant "the one
    /// live direction"; [`Settings::sanitise`] reads that older meaning into
    /// [`Settings::input_enabled`] when a file predates the split.
    pub device_direction: DeviceDirection,
    /// Whether the input lane comes up at start.
    ///
    /// An `Option` so that a 0.3.0 file can be told apart from a 0.4.0 one that says `false`: that
    /// version had one lane, and `device_direction = "input"` was how it said the microphone was
    /// the live one. [`Settings::sanitise`] turns the absence into `Some(device_direction ==
    /// Input)`, so a user who left 0.3.0 on a microphone comes back to it. Read through
    /// [`Settings::lane_enabled`], which applies the same rule before sanitising has run.
    pub input_enabled: Option<bool>,
    /// Whether the output lane comes up at start. `true` unless the user detached it: the
    /// Windows behaviour, and what a fresh install does.
    pub output_enabled: bool,
    /// Remembered devices and their presets, in both directions.
    pub device_configs: Vec<DeviceConfig>,
    /// Schema version of `device_configs`.
    pub device_configs_version: u32,
    /// Switch to a newly appeared output device automatically.
    pub prioritize_new_output: bool,
    /// What the session default was before FxSound took it, one per direction.
    ///
    /// The audio thread keeps this on its own heap and hands the default back on exit, on a
    /// direction switch and on `SIGTERM`/`SIGINT`. None of that survives a `SIGKILL`, an OOM kill
    /// or a power cut — and what those leave behind is a session default naming FxSound's virtual
    /// node, which is gone. Written here so the next start can repair it, which is the only place
    /// the repair can live: no signal handler runs for any of the three.
    pub remembered_default_output: String,
    pub remembered_default_input: String,

    // ---- microphone: global overrides ----------------------------------------------------------
    //
    // A voice preset carries its own denoiser level and channel mode; these sit over every
    // preset at once, and `Preset` means "whatever the preset says". Echo cancellation is global
    // because a PipeWire module is per session, not per preset.
    /// Noise-suppression level over every voice preset, or follow the preset.
    pub noise_suppression: NoiseSuppressionOverride,
    /// Denoiser channel mode over every voice preset, or follow the preset.
    pub denoise_channels: DenoiseChannelsOverride,
    /// Whether the de-esser's corner is the preset's or chosen from the source's bandwidth.
    pub deesser_mode: DeEsserMode,
    /// Run PipeWire's echo canceller in front of the input lane.
    pub echo_cancel: bool,
    /// Late-reverberation suppression on the input lane.
    pub dereverb: DereverbLevel,
    /// The calibration wizard's last result, for the Settings pane.
    pub calibration: Option<CalibrationRecord>,

    // ---- dsp state that lives outside the preset --------------------------------------------
    pub master_gain: f32,
    pub balance: f32,
    pub volume_leveling: f32,
    pub filter_q: f32,
    pub num_bands: u32,

    // ---- window ----------------------------------------------------------------------------
    pub view: ViewMode,
    pub theme_mode: ThemeMode,
    /// Kept for compatibility and for the X11 fallback. Under Wayland a client cannot position
    /// its own toplevel, so the compositor decides and these are only written, never applied.
    pub window_x: i32,
    pub window_y: i32,
    /// Requested by the app through `ViewportCommand::WindowLevel`; Hyprland honours it.
    pub always_on_top: bool,
    /// Start with no window, tray only.
    pub run_minimized: bool,

    // ---- ui --------------------------------------------------------------------------------
    /// UI language code (`"en"`, `"de"`, …) — the one the user picked in Settings. Only
    /// consulted while `language_follows_system` is `false`.
    pub language: String,
    /// Show the UI in the desktop session's language rather than in [`Settings::language`].
    /// Linux only: the Windows build has no such notion, so this is what a fresh install does.
    pub language_follows_system: bool,
    pub hide_help_tooltips: bool,
    pub hide_notifications: bool,
    /// Whether the shortcut rows in Settings are shown as active.
    pub hotkeys: bool,
    #[serde(flatten)]
    pub hotkey_bindings: Hotkeys,
    /// How many presets the user may save.
    pub max_user_presets: u32,

    // ---- housekeeping ----------------------------------------------------------------------
    /// Present so the Settings pane matches the original. This fork never contacts the network;
    /// the toggle is inert and the Help pane says so.
    pub automatic_updates: bool,
    pub last_update_time: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            power: true,
            output_preset: "General".into(),
            input_preset: "Clean Voice".into(),
            legacy_preset: None,
            output_device_name: String::new(),
            input_device_name: String::new(),
            device_direction: DeviceDirection::Output,
            // `None` and not `Some(false)`: the value is decided by `sanitise`, which is the only
            // place that can tell a fresh default from a 0.3.0 file — both are missing the key.
            input_enabled: None,
            output_enabled: true,
            device_configs: Vec::new(),
            device_configs_version: 2,
            prioritize_new_output: false,
            remembered_default_output: String::new(),
            remembered_default_input: String::new(),

            noise_suppression: NoiseSuppressionOverride::Preset,
            denoise_channels: DenoiseChannelsOverride::Preset,
            deesser_mode: DeEsserMode::Classic,
            echo_cancel: false,
            dereverb: DereverbLevel::Off,
            calibration: None,

            master_gain: 0.0,
            balance: 0.0,
            volume_leveling: 0.0,
            filter_q: 1.0,
            num_bands: crate::eq::DEFAULT_BANDS as u32,

            view: ViewMode::Pro,
            theme_mode: ThemeMode::Dark,
            window_x: 0,
            window_y: 0,
            always_on_top: false,
            run_minimized: false,

            language: "en".into(),
            language_follows_system: true,
            hide_help_tooltips: false,
            hide_notifications: false,
            hotkeys: true,
            hotkey_bindings: Hotkeys::default(),
            max_user_presets: 120,

            automatic_updates: false,
            last_update_time: 0,
        }
    }
}

impl Settings {
    /// `~/.config/fxsound` (or `$XDG_CONFIG_HOME/fxsound`).
    #[must_use]
    pub fn config_dir() -> std::path::PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("fxsound")
    }

    /// `~/.config/fxsound/settings.toml`.
    #[must_use]
    pub fn config_path() -> std::path::PathBuf {
        Self::config_dir().join("settings.toml")
    }

    /// `~/.local/share/fxsound/presets` — where imported and user-saved presets land.
    #[must_use]
    pub fn user_preset_dir() -> std::path::PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("fxsound")
            .join("presets")
    }

    /// Load the settings file, falling back to defaults for anything missing or unreadable.
    ///
    /// A corrupt file is never fatal: the user's audio should still come up.
    #[must_use]
    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    /// [`Settings::load`] from an explicit path — the seam the migration tests use.
    ///
    /// A file that does not parse is **moved aside**, to `<path>.bad`, before the defaults are
    /// returned. Silently resetting was the 0.3.0 behaviour, and it had a cost that grew with
    /// every key added: one typo in a hand edit — `power = "yes"` — and the next save wrote the
    /// defaults over every device, preset and language the user had, with only a log line to say
    /// so. Now the next save writes a fresh file beside the broken one, and the broken one is
    /// there to be read back. A file that cannot be *read* (permissions, a directory in its place)
    /// is left where it is: there is nothing to preserve that is not already preserved.
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("{}: {err}; using defaults", path.display());
                }
                return Self::sanitised_default();
            }
        };
        match toml::from_str::<Self>(&text) {
            Ok(mut settings) => {
                settings.sanitise();
                settings
            }
            Err(err) => {
                let aside = Self::bad_path(path);
                match std::fs::rename(path, &aside) {
                    Ok(()) => log::warn!(
                        "{}: {err}; moved aside as {} and using defaults",
                        path.display(),
                        aside.display()
                    ),
                    Err(rename_err) => log::warn!(
                        "{}: {err}; could not move it aside ({rename_err}); using defaults",
                        path.display()
                    ),
                }
                Self::sanitised_default()
            }
        }
    }

    /// Where [`Settings::load_from`] moves a file it could not parse: `settings.toml.bad` beside
    /// `settings.toml`.
    #[must_use]
    pub fn bad_path(path: &Path) -> std::path::PathBuf {
        let mut name = path.file_name().map_or_else(
            || std::ffi::OsString::from("settings.toml"),
            ToOwned::to_owned,
        );
        name.push(".bad");
        path.with_file_name(name)
    }

    /// The defaults as the loader hands them out: with the migration rule already applied, so a
    /// first run and a run that found no file agree with a run that found one.
    fn sanitised_default() -> Self {
        let mut settings = Self::default();
        settings.sanitise();
        settings
    }

    /// Write the settings file, creating the config directory if needed.
    ///
    /// Durable replace (`crate::atomic`): an interrupted save cannot truncate the existing
    /// settings, and the temporary file carries the process id rather than a fixed name, so two
    /// instances saving at once cannot write over each other's half-finished file.
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::config_path())
    }

    /// [`Settings::save`] to an explicit path — the seam the round-trip tests use.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        // Never write out a value the loader would have to repair; a `nan` in this file is
        // indistinguishable from one the user typed.
        let mut checked = self.clone();
        checked.sanitise();
        let text = toml::to_string_pretty(&checked)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        crate::atomic::write(path, text.as_bytes())
    }

    /// Force the DSP fields into the ranges the controls enforce.
    ///
    /// This file is the one path into the engine with no validation on it: every other route goes
    /// through a slider or a command-line parser, but `settings.toml` is plain TOML, which spells
    /// `nan` and `inf` as ordinary floats and accepts any magnitude. Run on load, so a corrupt or
    /// hand-edited file cannot reach a filter design, and on save, so the file never records a
    /// value the loader would have to fix again.
    pub fn sanitise(&mut self) {
        use crate::limits::{self, finite};

        let default = Self::default();

        // A 0.2.0 file's single preset is this version's *output* preset. It is taken only when
        // the new key is absent — which, with `#[serde(default)]` filling it in, reads as "still
        // the default". A file carrying both a stale `preset` line and a real `output_preset` is
        // already strange; the worst this rule can do there is prefer the older of the two names,
        // which beats the alternative of refusing to load the file at all.
        if let Some(legacy) = self.legacy_preset.take()
            && self.output_preset == default.output_preset
        {
            self.output_preset = legacy;
        }

        // A 0.3.0 file has no `input_enabled`, and its `device_direction` said which of the two
        // devices was the live one. A user who left that version on a microphone comes back to
        // the microphone; one who left it on the speakers gets the speakers and no input lane —
        // which is also what a fresh install gets. Only the absence is read this way: a file that
        // says `false` beside `device_direction = "input"` is a 0.4.0 file whose user detached
        // the microphone while still editing its chain, and is believed.
        if self.input_enabled.is_none() {
            self.input_enabled = Some(self.device_direction == DeviceDirection::Input);
        }

        // Informational, and dropped rather than clamped when corrupt: see `CalibrationRecord`.
        // A ratio is still held to being a ratio.
        if let Some(record) = &mut self.calibration {
            if record.is_finite() {
                record.clipped_ratio = record.clipped_ratio.clamp(0.0, 1.0);
            } else {
                self.calibration = None;
            }
        }

        self.master_gain = finite(
            self.master_gain,
            limits::MASTER_GAIN_DB,
            default.master_gain,
        );
        self.balance = finite(self.balance, limits::BALANCE_DB, default.balance);
        self.volume_leveling = finite(
            self.volume_leveling,
            limits::VOLUME_LEVELING,
            default.volume_leveling,
        );
        self.filter_q = finite(self.filter_q, limits::FILTER_Q, default.filter_q);
        self.num_bands = self.num_bands.clamp(1, crate::eq::MAX_BANDS as u32);
    }

    /// The `node.name` one lane should attach to on launch. Empty when nothing was ever chosen,
    /// in which case the engine's own device rules pick.
    #[must_use]
    pub fn device_name(&self, direction: DeviceDirection) -> &str {
        match direction {
            DeviceDirection::Output => &self.output_device_name,
            DeviceDirection::Input => &self.input_device_name,
        }
    }

    /// Remember a device choice for one lane. The other lane's memory is kept, and the edit
    /// direction does not move: which chain the window edits is a separate decision
    /// ([`Settings::set_edit_direction`]), and coupling the two was the 0.3.0 bug that made
    /// picking a microphone silently re-point every control at it.
    pub fn set_device_name(&mut self, direction: DeviceDirection, node_name: &str) {
        match direction {
            DeviceDirection::Output => node_name.clone_into(&mut self.output_device_name),
            DeviceDirection::Input => node_name.clone_into(&mut self.input_device_name),
        }
    }

    /// Which chain the window edits. Never touches a device name or a lane's enabled state.
    pub fn set_edit_direction(&mut self, direction: DeviceDirection) {
        self.device_direction = direction;
    }

    /// Whether a lane comes up at start.
    ///
    /// For the input lane this applies the 0.3.0 migration rule even before
    /// [`Settings::sanitise`] has run, so the answer is the same whichever order a caller asks in.
    #[must_use]
    pub fn lane_enabled(&self, direction: DeviceDirection) -> bool {
        match direction {
            DeviceDirection::Output => self.output_enabled,
            DeviceDirection::Input => self
                .input_enabled
                .unwrap_or(self.device_direction == DeviceDirection::Input),
        }
    }

    /// Record whether a lane comes up at start.
    pub fn set_lane_enabled(&mut self, direction: DeviceDirection, enabled: bool) {
        match direction {
            DeviceDirection::Output => self.output_enabled = enabled,
            DeviceDirection::Input => self.input_enabled = Some(enabled),
        }
    }

    /// What the session default was before FxSound took this direction.
    #[must_use]
    pub fn remembered_default(&self, direction: DeviceDirection) -> &str {
        match direction {
            DeviceDirection::Output => &self.remembered_default_output,
            DeviceDirection::Input => &self.remembered_default_input,
        }
    }

    /// Record what the session default was before FxSound took this direction.
    pub fn set_remembered_default(&mut self, direction: DeviceDirection, node_name: &str) {
        match direction {
            DeviceDirection::Output => node_name.clone_into(&mut self.remembered_default_output),
            DeviceDirection::Input => node_name.clone_into(&mut self.remembered_default_input),
        }
    }

    /// The preset belonging to one direction.
    #[must_use]
    pub fn preset_for_direction(&self, direction: DeviceDirection) -> &str {
        match direction {
            DeviceDirection::Output => &self.output_preset,
            DeviceDirection::Input => &self.input_preset,
        }
    }

    /// Record the preset for one direction, leaving the other's alone.
    pub fn set_preset_for_direction(&mut self, direction: DeviceDirection, name: &str) {
        match direction {
            DeviceDirection::Output => name.clone_into(&mut self.output_preset),
            DeviceDirection::Input => name.clone_into(&mut self.input_preset),
        }
    }

    /// The preset of the edit direction — what the picker shows on launch.
    #[must_use]
    pub fn selected_preset(&self) -> &str {
        self.preset_for_direction(self.device_direction)
    }

    /// Record the preset for the edit direction, leaving the other direction's alone.
    pub fn set_selected_preset(&mut self, name: &str) {
        self.set_preset_for_direction(self.device_direction, name);
    }

    /// The 0.3.0 spelling of [`Settings::set_device_name`], argument order and all.
    ///
    /// It no longer moves the edit direction. In 0.3.0 it did, as a side effect, and every caller
    /// that wanted the direction to follow the device relied on it silently; now a caller says so
    /// with [`Settings::set_edit_direction`].
    pub fn set_selected_device(&mut self, node_name: &str, direction: DeviceDirection) {
        self.set_device_name(direction, node_name);
    }

    /// The language code the UI should be shown in right now: the system's, or the explicit
    /// pick when there is one and a translation table exists for it.
    #[must_use]
    pub fn effective_language(&self) -> &'static str {
        crate::i18n::resolve(self.language_follows_system, &self.language)
    }

    /// Record a language pick from Settings. `None` means "follow the system".
    pub fn choose_language(&mut self, code: Option<&str>) {
        match code {
            Some(code) => {
                code.clone_into(&mut self.language);
                self.language_follows_system = false;
            }
            None => self.language_follows_system = true,
        }
    }

    /// The preset remembered for a device, if this device has been seen before in this
    /// direction.
    ///
    /// Keyed on the direction as well as the name: the two lanes' presets come from different
    /// stores, so a memory of the output lane's `General` must never answer for a microphone.
    #[must_use]
    pub fn preset_for_device(&self, node_name: &str, direction: DeviceDirection) -> Option<&str> {
        self.device_configs
            .iter()
            .find(|c| c.device_id == node_name && c.direction == direction)
            .map(|c| c.preset.as_str())
    }

    /// Remember which preset was in use for a device.
    pub fn remember_device_preset(
        &mut self,
        node_name: &str,
        description: &str,
        preset: &str,
        form_factor: &str,
        direction: DeviceDirection,
    ) {
        if let Some(existing) = self
            .device_configs
            .iter_mut()
            .find(|c| c.device_id == node_name && c.direction == direction)
        {
            existing.device_name = description.to_owned();
            existing.preset = preset.to_owned();
            // A device can change what it looks like: a USB dock's port is `line-level` until its
            // profile says otherwise, and a Bluetooth headset swaps between `headset` and
            // `headphone` with its profile.
            if !form_factor.is_empty() {
                existing.device_form_factor = form_factor.to_owned();
            }
        } else {
            self.device_configs.push(DeviceConfig {
                device_id: node_name.to_owned(),
                direction,
                device_name: description.to_owned(),
                preset: preset.to_owned(),
                device_form_factor: form_factor.to_owned(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let original = Settings::default();
        let text = toml::to_string_pretty(&original).expect("serialise");
        let parsed: Settings = toml::from_str(&text).expect("parse");
        assert_eq!(original, parsed);
    }

    #[test]
    fn an_empty_file_yields_defaults() {
        let parsed: Settings = toml::from_str("").expect("parse empty");
        assert_eq!(parsed, Settings::default());
    }

    #[test]
    fn unknown_keys_do_not_break_loading() {
        let mut parsed: Settings =
            toml::from_str("preset = \"Jazz\"\nsome_future_key = 3\n").expect("parse");
        parsed.sanitise();
        assert_eq!(parsed.output_preset, "Jazz");
        assert_eq!(parsed.max_user_presets, 120);
    }

    #[test]
    fn a_settings_file_written_by_0_2_0_still_finds_its_preset() {
        // 0.2.0 had one preset key because it had one chain. The alias puts it where the playback
        // direction now looks, and the microphone starts from the default rather than inheriting
        // a music preset it was never meant to have.
        let parsed: Settings = toml::from_str(
            "preset = \"Rock\"\noutput_device_name = \"alsa_output.pci-0000_00_1f.3\"\n",
        )
        .expect("parse");
        let mut parsed = parsed;
        parsed.sanitise();
        assert_eq!(parsed.output_preset, "Rock");
        assert_eq!(
            parsed.selected_preset(),
            "Rock",
            "0.2.0 files are output files"
        );
        assert_eq!(parsed.input_preset, Settings::default().input_preset);

        // And a file carrying both keys still loads — which is the whole reason this is a field
        // rather than a `#[serde(alias)]`. With an alias serde called it a duplicate field,
        // `load` turned that into a full reset, and one stale line cost the user everything.
        let mut both: Settings =
            toml::from_str("preset = \"Rock\"\noutput_preset = \"Jazz\"\n").expect("parse");
        both.sanitise();
        assert_eq!(both.output_preset, "Jazz");
    }

    #[test]
    fn each_direction_remembers_the_default_it_displaced() {
        // The memory that has to outlive the process. A SIGKILL, an OOM kill and a power cut all
        // run no signal handler, so what they leave behind is a session default naming FxSound's
        // node, which is gone — and the only place to repair that is the next start.
        let mut s = Settings::default();
        assert_eq!(s.remembered_default(crate::DeviceDirection::Output), "");

        s.set_remembered_default(
            crate::DeviceDirection::Output,
            "alsa_output.pci-0000_00_1f.3",
        );
        s.set_remembered_default(crate::DeviceDirection::Input, "alsa_input.usb-fifine");
        assert_eq!(
            s.remembered_default(crate::DeviceDirection::Output),
            "alsa_output.pci-0000_00_1f.3"
        );
        assert_eq!(
            s.remembered_default(crate::DeviceDirection::Input),
            "alsa_input.usb-fifine"
        );

        // And it survives the file, which is the whole point.
        let text = toml::to_string_pretty(&s).expect("serialise");
        let mut back: Settings = toml::from_str(&text).expect("parse");
        back.sanitise();
        assert_eq!(
            back.remembered_default(crate::DeviceDirection::Output),
            "alsa_output.pci-0000_00_1f.3"
        );
    }

    #[test]
    fn the_two_directions_remember_their_own_presets() {
        let mut s = Settings::default();
        s.set_device_name(DeviceDirection::Output, "alsa_output.pci");
        s.set_edit_direction(DeviceDirection::Output);
        s.set_selected_preset("Rock");
        assert_eq!(s.selected_preset(), "Rock");

        s.set_device_name(DeviceDirection::Input, "alsa_input.usb-fifine");
        s.set_edit_direction(DeviceDirection::Input);
        assert_eq!(
            s.selected_preset(),
            "Clean Voice",
            "a microphone must not inherit the music preset"
        );
        s.set_selected_preset("Clean Voice");

        // And switching back returns what was there, in both senses.
        s.set_edit_direction(DeviceDirection::Output);
        assert_eq!(s.selected_preset(), "Rock");
        assert_eq!(
            s.preset_for_direction(DeviceDirection::Input),
            "Clean Voice"
        );
        assert_eq!(s.device_name(DeviceDirection::Output), "alsa_output.pci");
        assert_eq!(
            s.device_name(DeviceDirection::Input),
            "alsa_input.usb-fifine"
        );
    }

    #[test]
    fn choosing_a_device_no_longer_moves_the_edit_direction() {
        // The 0.3.0 coupling: `set_selected_device` flipped `device_direction` as a side effect,
        // and picking a microphone silently re-pointed every control at it. Both spellings now
        // leave the edit direction where it was.
        let mut s = Settings::default();
        s.set_selected_device("alsa_input.usb-fifine", DeviceDirection::Input);
        assert_eq!(s.device_direction, DeviceDirection::Output);
        assert_eq!(
            s.device_name(DeviceDirection::Input),
            "alsa_input.usb-fifine"
        );
        s.set_device_name(DeviceDirection::Input, "alsa_input.other");
        assert_eq!(s.device_direction, DeviceDirection::Output);

        s.set_edit_direction(DeviceDirection::Input);
        assert_eq!(s.device_direction, DeviceDirection::Input);
        // And the edit direction moves nothing else.
        assert_eq!(s.device_name(DeviceDirection::Input), "alsa_input.other");
        assert_eq!(s.device_name(DeviceDirection::Output), "");
        assert!(s.output_enabled);
        assert_eq!(s.input_enabled, None);
    }

    #[test]
    fn device_presets_are_remembered_and_updated() {
        let mut s = Settings::default();
        s.remember_device_preset(
            "alsa_output.pci-0000_00_1f.3",
            "Speakers",
            "Rock",
            "speaker",
            DeviceDirection::Output,
        );
        assert_eq!(
            s.preset_for_device("alsa_output.pci-0000_00_1f.3", DeviceDirection::Output),
            Some("Rock")
        );
        s.remember_device_preset(
            "alsa_output.pci-0000_00_1f.3",
            "Speakers",
            "Jazz",
            "headphone",
            DeviceDirection::Output,
        );
        assert_eq!(s.device_configs.len(), 1);
        assert_eq!(
            s.preset_for_device("alsa_output.pci-0000_00_1f.3", DeviceDirection::Output),
            Some("Jazz")
        );
        assert_eq!(
            s.device_configs[0].device_form_factor, "headphone",
            "a device may change what it looks like"
        );
    }

    #[test]
    fn a_device_memory_belongs_to_one_direction() {
        // The two lanes' presets come from different stores and their names can collide, so the
        // same node name in the two directions is two memories, and one never answers for the
        // other.
        let mut s = Settings::default();
        s.remember_device_preset("dock", "Dock", "Rock", "", DeviceDirection::Output);
        s.remember_device_preset("dock", "Dock", "Headset", "", DeviceDirection::Input);
        assert_eq!(s.device_configs.len(), 2);
        assert_eq!(
            s.preset_for_device("dock", DeviceDirection::Output),
            Some("Rock")
        );
        assert_eq!(
            s.preset_for_device("dock", DeviceDirection::Input),
            Some("Headset")
        );
        assert_eq!(
            s.preset_for_device("alsa_input.usb-fifine", DeviceDirection::Input),
            None
        );
        // Updating one leaves the other alone.
        s.remember_device_preset("dock", "Dock", "Jazz", "", DeviceDirection::Output);
        assert_eq!(s.device_configs.len(), 2);
        assert_eq!(
            s.preset_for_device("dock", DeviceDirection::Input),
            Some("Headset")
        );
    }

    #[test]
    fn a_device_config_without_a_direction_reads_as_an_output() {
        // 0.3.0 wrote only output devices into `device_configs`, without saying so: selecting a
        // voice preset returned before the memory was written (`App::select_preset`), so an
        // entry without the key can only be an output's, and reading it as one loses nothing.
        let parsed: Settings = toml::from_str(
            "[[device_configs]]\ndevice_id = \"alsa_output.pci\"\ndevice_name = \"Speakers\"\npreset = \"Rock\"\ndevice_form_factor = \"speaker\"\n",
        )
        .expect("parse");
        assert_eq!(parsed.device_configs.len(), 1);
        assert_eq!(parsed.device_configs[0].direction, DeviceDirection::Output);
        assert_eq!(
            parsed.preset_for_device("alsa_output.pci", DeviceDirection::Output),
            Some("Rock")
        );
        assert_eq!(
            parsed.preset_for_device("alsa_output.pci", DeviceDirection::Input),
            None
        );
        // And a 0.4.0 file that says so is believed.
        let parsed: Settings = toml::from_str(
            "[[device_configs]]\ndevice_id = \"alsa_input.usb\"\ndirection = \"input\"\npreset = \"Headset\"\n",
        )
        .expect("parse");
        assert_eq!(parsed.device_configs[0].direction, DeviceDirection::Input);
    }

    #[test]
    fn a_hand_edited_file_cannot_put_a_non_finite_value_into_the_dsp() {
        // TOML spells these as ordinary floats, and this file is the only route into the engine
        // with no slider or argument parser in front of it.
        let text = "\
master_gain = nan
balance = inf
volume_leveling = -inf
filter_q = nan
num_bands = 4000000
";
        let mut parsed: Settings = toml::from_str(text).expect("TOML accepts nan and inf");
        assert!(
            parsed.master_gain.is_nan(),
            "the hazard is real before sanitising"
        );

        parsed.sanitise();
        let default = Settings::default();
        // Non-finite falls back to the default rather than clamping: +inf on the master gain
        // would otherwise mean "maximum boost" to a user whose file was merely corrupted.
        assert_eq!(parsed.master_gain, default.master_gain);
        assert_eq!(parsed.balance, default.balance);
        assert_eq!(parsed.volume_leveling, default.volume_leveling);
        assert_eq!(parsed.filter_q, default.filter_q);
        assert_eq!(parsed.num_bands, crate::eq::MAX_BANDS as u32);
    }

    #[test]
    fn a_0_3_0_file_that_was_on_a_microphone_comes_back_to_the_microphone() {
        // 0.3.0 had one lane, and `device_direction = "input"` was how it said the microphone
        // was the live one. The output device it remembered is kept: the user is coming back to
        // both lanes, and the speakers should be the speakers they had.
        let text = "\
device_direction = \"input\"
output_device_name = \"alsa_output.pci-0000_00_1f.3\"
input_device_name = \"alsa_input.usb-fifine\"
output_preset = \"Rock\"
input_preset = \"Headset\"
";
        let mut parsed: Settings = toml::from_str(text).expect("parse");
        assert_eq!(
            parsed.input_enabled, None,
            "the key is absent before sanitise"
        );
        assert!(
            parsed.lane_enabled(DeviceDirection::Input),
            "the accessor applies the rule even before sanitise"
        );
        parsed.sanitise();
        assert_eq!(parsed.input_enabled, Some(true));
        assert!(parsed.output_enabled);
        assert!(parsed.lane_enabled(DeviceDirection::Input));
        assert!(parsed.lane_enabled(DeviceDirection::Output));
        assert_eq!(
            parsed.device_direction,
            DeviceDirection::Input,
            "still the edit direction"
        );
        assert_eq!(
            parsed.device_name(DeviceDirection::Output),
            "alsa_output.pci-0000_00_1f.3"
        );
        assert_eq!(
            parsed.device_name(DeviceDirection::Input),
            "alsa_input.usb-fifine"
        );
        assert_eq!(parsed.selected_preset(), "Headset");
    }

    #[test]
    fn a_0_3_0_file_that_was_on_the_speakers_gets_no_input_lane() {
        let text = "device_direction = \"output\"\ninput_device_name = \"alsa_input.usb-fifine\"\n";
        let mut parsed: Settings = toml::from_str(text).expect("parse");
        assert!(!parsed.lane_enabled(DeviceDirection::Input));
        parsed.sanitise();
        assert_eq!(parsed.input_enabled, Some(false));
        assert!(parsed.output_enabled);
        // The microphone it remembered is still remembered, for when the lane is enabled.
        assert_eq!(
            parsed.device_name(DeviceDirection::Input),
            "alsa_input.usb-fifine"
        );
        // And a file with no direction at all is a 0.2.0 file, which is an output file.
        let mut older: Settings = toml::from_str("preset = \"Rock\"\n").expect("parse");
        older.sanitise();
        assert_eq!(older.input_enabled, Some(false));
    }

    #[test]
    fn an_explicit_input_enabled_is_believed_whatever_the_edit_direction_says() {
        // A 0.4.0 file whose user detached the microphone while still editing its chain.
        let text = "device_direction = \"input\"\ninput_enabled = false\n";
        let mut parsed: Settings = toml::from_str(text).expect("parse");
        parsed.sanitise();
        assert_eq!(parsed.input_enabled, Some(false));
        assert!(!parsed.lane_enabled(DeviceDirection::Input));
        // And the converse: the lane on while the window edits the speakers.
        let text = "device_direction = \"output\"\ninput_enabled = true\noutput_enabled = false\n";
        let mut parsed: Settings = toml::from_str(text).expect("parse");
        parsed.sanitise();
        assert_eq!(parsed.input_enabled, Some(true));
        assert!(!parsed.output_enabled);
        assert!(!parsed.lane_enabled(DeviceDirection::Output));
    }

    #[test]
    fn the_lane_switches_are_written_and_read_back() {
        let mut s = Settings::default();
        s.set_lane_enabled(DeviceDirection::Output, false);
        s.set_lane_enabled(DeviceDirection::Input, true);
        assert!(!s.lane_enabled(DeviceDirection::Output));
        assert!(s.lane_enabled(DeviceDirection::Input));
        let text = toml::to_string_pretty(&s).expect("serialise");
        assert!(text.contains("input_enabled = true"), "{text}");
        assert!(text.contains("output_enabled = false"), "{text}");
        let back: Settings = toml::from_str(&text).expect("parse");
        assert_eq!(back.input_enabled, Some(true));
        assert!(!back.output_enabled);
    }

    #[test]
    fn a_default_file_is_written_with_the_migration_already_decided() {
        // `save` sanitises, so the file it writes carries `input_enabled` explicitly and the
        // next load never has to guess from the edit direction again.
        let text = toml::to_string_pretty(&{
            let mut s = Settings::default();
            s.sanitise();
            s
        })
        .expect("serialise");
        assert!(text.contains("input_enabled = false"), "{text}");
        // Whereas the raw default omits it, which is what lets `sanitise` see the absence.
        let raw = toml::to_string_pretty(&Settings::default()).expect("serialise");
        assert!(!raw.contains("input_enabled"), "{raw}");
    }

    #[test]
    fn a_file_that_does_not_parse_is_moved_aside_rather_than_silently_reset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let broken = "power = \"yes\"\noutput_preset = \"Rock\"\n";
        std::fs::write(&path, broken).expect("write");

        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, {
            let mut d = Settings::default();
            d.sanitise();
            d
        });

        let aside = dir.path().join("settings.toml.bad");
        assert_eq!(Settings::bad_path(&path), aside);
        assert_eq!(
            std::fs::read_to_string(&aside).expect("the broken file was moved aside"),
            broken,
            "byte for byte, so the hand edit can be read back"
        );
        assert!(
            !path.exists(),
            "and nothing is left to be overwritten in place"
        );

        // The next save writes a fresh file beside it and leaves the evidence alone.
        loaded.save_to(&path).expect("save");
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(&aside).expect("still there"),
            broken
        );
    }

    #[test]
    fn a_missing_file_yields_sanitised_defaults_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.toml");
        let loaded = Settings::load_from(&path);
        assert_eq!(
            loaded.input_enabled,
            Some(false),
            "the loader's defaults are sanitised"
        );
        assert_eq!(loaded.output_preset, "General");
        assert!(!path.exists());
        assert!(!Settings::bad_path(&path).exists());
    }

    #[test]
    fn every_new_key_survives_a_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("settings.toml");
        let original = Settings {
            input_enabled: Some(true),
            output_enabled: false,
            device_direction: DeviceDirection::Input,
            noise_suppression: NoiseSuppressionOverride::Strong,
            denoise_channels: DenoiseChannelsOverride::Linked,
            deesser_mode: DeEsserMode::Adaptive,
            echo_cancel: true,
            dereverb: DereverbLevel::Medium,
            calibration: Some(CalibrationRecord {
                noise_floor_db: -52.5,
                speech_rms_db: -21.0,
                speech_peak_db: -6.5,
                clipped_ratio: 0.001,
                unix_time: 1_760_000_000,
                preset: "Calibrated — Fifine K669".to_owned(),
                device: "alsa_input.usb-fifine".to_owned(),
            }),
            device_configs: vec![DeviceConfig {
                device_id: "alsa_input.usb-fifine".to_owned(),
                direction: DeviceDirection::Input,
                device_name: "Fifine K669".to_owned(),
                preset: "Calibrated — Fifine K669".to_owned(),
                device_form_factor: "microphone".to_owned(),
            }],
            ..Settings::default()
        };
        original.save_to(&path).expect("save creates the directory");
        let back = Settings::load_from(&path);
        assert_eq!(back, original);

        // And the keys are spelled the way the design record and the CLI spell them.
        let text = std::fs::read_to_string(&path).expect("read");
        for line in [
            "noise_suppression = \"strong\"",
            "denoise_channels = \"linked\"",
            "deesser_mode = \"adaptive\"",
            "echo_cancel = true",
            "dereverb = \"medium\"",
            "input_enabled = true",
            "output_enabled = false",
            "device_direction = \"input\"",
            "direction = \"input\"",
            "[calibration]",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
    }

    #[test]
    fn a_0_4_0_file_is_still_readable_by_the_keys_alone() {
        // Spelled by hand, the way the design record spells them, so a rename of a variant or a
        // change of `rename_all` cannot pass the round-trip test and still break the file.
        let text = "\
noise_suppression = \"light\"
denoise_channels = \"mono\"
deesser_mode = \"adaptive\"
echo_cancel = true
dereverb = \"strong\"

[calibration]
noise_floor_db = -48.0
speech_rms_db = -20.0
speech_peak_db = -4.0
clipped_ratio = 0.0
unix_time = 1
preset = \"Calibrated — Mic\"
device = \"mic\"
";
        let mut parsed: Settings = toml::from_str(text).expect("parse");
        parsed.sanitise();
        assert_eq!(parsed.noise_suppression, NoiseSuppressionOverride::Light);
        assert_eq!(parsed.denoise_channels, DenoiseChannelsOverride::Mono);
        assert_eq!(parsed.deesser_mode, DeEsserMode::Adaptive);
        assert!(parsed.echo_cancel);
        assert_eq!(parsed.dereverb, DereverbLevel::Strong);
        let record = parsed.calibration.expect("record");
        assert_eq!(record.noise_floor_db, -48.0);
        assert_eq!(record.preset, "Calibrated — Mic");
        assert_eq!(record.device, "mic");
        // A partial record fills in the rest rather than failing the whole file.
        let partial: Settings =
            toml::from_str("[calibration]\nnoise_floor_db = -40.0\n").expect("parse");
        assert_eq!(
            partial.calibration,
            Some(CalibrationRecord {
                noise_floor_db: -40.0,
                ..CalibrationRecord::default()
            })
        );
    }

    #[test]
    fn a_corrupt_calibration_record_is_dropped_and_a_ratio_stays_a_ratio() {
        let mut nan: Settings =
            toml::from_str("[calibration]\nnoise_floor_db = nan\n").expect("TOML accepts nan");
        assert!(
            nan.calibration.is_some(),
            "the hazard is real before sanitising"
        );
        nan.sanitise();
        assert_eq!(nan.calibration, None, "a floor of NaN is not a measurement");

        let mut inf: Settings =
            toml::from_str("[calibration]\nspeech_rms_db = -inf\n").expect("parse");
        inf.sanitise();
        assert_eq!(inf.calibration, None);

        let mut ratio: Settings =
            toml::from_str("[calibration]\nclipped_ratio = 7.5\n").expect("parse");
        ratio.sanitise();
        assert_eq!(
            ratio.calibration.map(|r| r.clipped_ratio),
            Some(1.0),
            "finite but impossible is clamped, as everywhere else"
        );
        let mut fine: Settings =
            toml::from_str("[calibration]\nclipped_ratio = 0.25\nunix_time = 9\n").expect("parse");
        fine.sanitise();
        assert_eq!(
            fine.calibration.as_ref().map(|r| r.clipped_ratio),
            Some(0.25)
        );
        assert_eq!(fine.calibration.map(|r| r.unix_time), Some(9));
    }

    #[test]
    fn the_microphone_overrides_default_to_following_the_preset() {
        let s = Settings::default();
        assert_eq!(s.noise_suppression, NoiseSuppressionOverride::Preset);
        assert_eq!(s.denoise_channels, DenoiseChannelsOverride::Preset);
        assert_eq!(s.deesser_mode, DeEsserMode::Classic);
        assert!(!s.echo_cancel);
        assert_eq!(s.dereverb, DereverbLevel::Off);
        assert_eq!(s.calibration, None);
        assert!(s.output_enabled);
        // An empty file — a fresh install — agrees.
        let mut fresh: Settings = toml::from_str("").expect("parse");
        fresh.sanitise();
        assert_eq!(fresh.noise_suppression, NoiseSuppressionOverride::Preset);
        assert_eq!(fresh.input_enabled, Some(false));
    }

    #[test]
    fn out_of_range_values_are_pulled_back_to_what_the_controls_allow() {
        let mut s = Settings {
            master_gain: 500.0,
            balance: -99.0,
            volume_leveling: 12.0,
            filter_q: 0.1,
            num_bands: 0,
            ..Settings::default()
        };
        s.sanitise();
        assert_eq!(s.master_gain, 20.0);
        assert_eq!(s.balance, -20.0);
        assert_eq!(s.volume_leveling, 4.0);
        assert_eq!(s.filter_q, 1.0);
        assert_eq!(s.num_bands, 1);
    }
}
