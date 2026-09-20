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

use serde::{Deserialize, Serialize};

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

/// One remembered output device and the preset the user last used with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceConfig {
    /// On Windows an endpoint id; here the PipeWire `node.name`, which is stable across restarts.
    pub device_id: String,
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
    /// chose. Until the input preset set ships this defaults to the same name as the output one,
    /// because there is only one pool of presets to name.
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
    /// `node.name` of the real capture device FxSound listens to, when it runs as an input.
    pub input_device_name: String,
    /// Which of the two devices above is the live one. Linux only: the Windows build has no
    /// capture mode.
    pub device_direction: crate::DeviceDirection,
    /// Remembered devices and their presets.
    pub device_configs: Vec<DeviceConfig>,
    /// Schema version of `device_configs`.
    pub device_configs_version: u32,
    /// Switch to a newly appeared output device automatically.
    pub prioritize_new_output: bool,

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
            input_preset: "General".into(),
            legacy_preset: None,
            output_device_name: String::new(),
            input_device_name: String::new(),
            device_direction: crate::DeviceDirection::Output,
            device_configs: Vec::new(),
            device_configs_version: 2,
            prioritize_new_output: false,

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
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Self>(&text) {
                Ok(mut settings) => {
                    settings.sanitise();
                    settings
                }
                Err(err) => {
                    log::warn!("{}: {err}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                log::warn!("{}: {err}; using defaults", path.display());
                Self::default()
            }
        }
    }

    /// Write the settings file, creating the config directory if needed.
    ///
    /// Durable replace (`crate::atomic`): an interrupted save cannot truncate the existing
    /// settings, and the temporary file carries the process id rather than a fixed name, so two
    /// instances saving at once cannot write over each other's half-finished file.
    pub fn save(&self) -> std::io::Result<()> {
        // Never write out a value the loader would have to repair; a `nan` in this file is
        // indistinguishable from one the user typed.
        let mut checked = self.clone();
        checked.sanitise();
        let text = toml::to_string_pretty(&checked)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        crate::atomic::write(&Self::config_path(), text.as_bytes())
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

    /// The `node.name` FxSound should attach to on launch, for the saved direction.
    #[must_use]
    pub fn selected_device_name(&self) -> &str {
        match self.device_direction {
            crate::DeviceDirection::Output => &self.output_device_name,
            crate::DeviceDirection::Input => &self.input_device_name,
        }
    }

    /// The preset belonging to one direction.
    #[must_use]
    pub fn preset_for_direction(&self, direction: crate::DeviceDirection) -> &str {
        match direction {
            crate::DeviceDirection::Output => &self.output_preset,
            crate::DeviceDirection::Input => &self.input_preset,
        }
    }

    /// The preset of the direction FxSound is in right now — what to select on launch.
    #[must_use]
    pub fn selected_preset(&self) -> &str {
        self.preset_for_direction(self.device_direction)
    }

    /// Record the preset for the live direction, leaving the other direction's alone.
    pub fn set_selected_preset(&mut self, name: &str) {
        match self.device_direction {
            crate::DeviceDirection::Output => name.clone_into(&mut self.output_preset),
            crate::DeviceDirection::Input => name.clone_into(&mut self.input_preset),
        }
    }

    /// Remember a device choice. The other direction's memory is kept, so switching back to an
    /// output after trying a microphone returns to the output the user had.
    pub fn set_selected_device(&mut self, node_name: &str, direction: crate::DeviceDirection) {
        match direction {
            crate::DeviceDirection::Output => node_name.clone_into(&mut self.output_device_name),
            crate::DeviceDirection::Input => node_name.clone_into(&mut self.input_device_name),
        }
        self.device_direction = direction;
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

    /// The preset remembered for a device, if this device has been seen before.
    #[must_use]
    pub fn preset_for_device(&self, node_name: &str) -> Option<&str> {
        self.device_configs
            .iter()
            .find(|c| c.device_id == node_name)
            .map(|c| c.preset.as_str())
    }

    /// Remember which preset was in use for a device.
    pub fn remember_device_preset(
        &mut self,
        node_name: &str,
        description: &str,
        preset: &str,
        form_factor: &str,
    ) {
        if let Some(existing) = self
            .device_configs
            .iter_mut()
            .find(|c| c.device_id == node_name)
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
    fn the_two_directions_remember_their_own_presets() {
        let mut s = Settings::default();
        s.set_selected_device("alsa_output.pci", crate::DeviceDirection::Output);
        s.set_selected_preset("Rock");
        assert_eq!(s.selected_preset(), "Rock");

        s.set_selected_device("alsa_input.usb-fifine", crate::DeviceDirection::Input);
        assert_eq!(
            s.selected_preset(),
            "General",
            "a microphone must not inherit the music preset"
        );
        s.set_selected_preset("Clean Voice");

        // And switching back returns what was there, in both senses.
        s.set_selected_device("alsa_output.pci", crate::DeviceDirection::Output);
        assert_eq!(s.selected_preset(), "Rock");
        assert_eq!(
            s.preset_for_direction(crate::DeviceDirection::Input),
            "Clean Voice"
        );
    }

    #[test]
    fn device_presets_are_remembered_and_updated() {
        let mut s = Settings::default();
        s.remember_device_preset(
            "alsa_output.pci-0000_00_1f.3",
            "Speakers",
            "Rock",
            "speaker",
        );
        assert_eq!(
            s.preset_for_device("alsa_output.pci-0000_00_1f.3"),
            Some("Rock")
        );
        s.remember_device_preset(
            "alsa_output.pci-0000_00_1f.3",
            "Speakers",
            "Jazz",
            "headphone",
        );
        assert_eq!(s.device_configs.len(), 1);
        assert_eq!(
            s.preset_for_device("alsa_output.pci-0000_00_1f.3"),
            Some("Jazz")
        );
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
