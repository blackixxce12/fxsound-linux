//! The command line.
//!
//! The Windows build parses its command line twice, with two different option sets:
//! `FxController::initConfig` (`fxsound/Source/GUI/FxController.cpp:221-342`) runs in the first
//! instance before any UI exists, and `FxController::applyConfig` (`:344-602`) runs in the
//! *already running* instance when a second process is launched and JUCE forwards its command line
//! (`fxsound/Source/Main.cpp:136-139`). The union of both is documented normatively in
//! `docs/COMMAND_LINE_OPTIONS.md`, and this module keeps every option spelled exactly as that
//! document spells it — underscores included — so existing scripts and the `fxmcp` MCP server keep
//! working against the port.
//!
//! Three deliberate departures from the original, each one prescribed by
//! `docs/spec/07-startup-tray.md` §4.7, and four the 0.4.0 audit added:
//!
//! 1. **A space works as well as an `=`.** `docs/COMMAND_LINE_OPTIONS.md:7` says `--power 1` parses
//!    as two unrelated arguments and the value is silently ignored. That is an artefact of JUCE
//!    re-tokenising the raw `GetCommandLineW()` tail, not a design decision, and it is
//!    user-hostile in a shell. `clap` accepts both forms and we keep that.
//! 2. **Out-of-range numbers are an error, not a silent reset.** `docs/COMMAND_LINE_OPTIONS.md:9`
//!    resets them to the default with no message, which is a bug generator; the in-tree Go client
//!    already validates instead (`fxmcp/internal/fxsound/config.go:186-202`). Silent clamping is
//!    kept only for values read back from the settings file, which is `fxsound-core`'s job.
//! 3. **Linux additions for the compositor.** A Wayland client cannot grab a global hotkey
//!    (`docs/spec/07-startup-tray.md` §2.7), so the five `RegisterHotKey` bindings become
//!    compositor keybindings that run this CLI: `--toggle-power`, `--next-preset`,
//!    `--prev-preset`, `--next-output` and `--toggle-window`, plus `--show`, `--hide` and
//!    `--quit`. `packaging/hyprland.conf.example` binds five of them, on Super+Alt: a compositor
//!    binding takes its keys from every application, and the Windows build's own chords collide
//!    (Ctrl+Shift+Q is how Chromium quits; 0.4.0 audit #33).
//! 4. **Only what is about the window raises it** (0.4.0 audit R11). On Windows every forwarded
//!    command line but `--status` shows and raises the window (`FxController.cpp:523-531`), so a
//!    keybind that runs `fxsound --preset=Gaming` pulled the window over the game each time. Here
//!    an option that sets something — a preset, the power, a device, an effect, a band, a level,
//!    the language — does it silently, and a line of nothing else that starts FxSound starts it in
//!    the tray ([`Cli::only_sets_things`]); `--show`, `--view` and a line with no options at all
//!    raise the window, `--toggle-window` toggles it and `--hide` hides it
//!    ([`Cli::window_command`]). At a start `--view` sets the layout and the window follows the
//!    remembered tray state, unless `--show` is given too ([`Cli::cold_start_commands`]).
//! 5. **One preset option per line** (audit #30). Windows takes the first of `--preset`,
//!    `--save_preset`, `--overwrite_preset`, `--undo_preset`, `--rename_preset` and
//!    `--delete_preset` and silently drops the rest (`FxController.cpp:393-448`); here a second one
//!    — `--next-preset` and `--prev-preset` included — is a parse error naming both, and so is a
//!    name that is empty, or one that has nothing left once the characters a preset name cannot
//!    hold are taken out.
//! 6. **A band list with a band the equalizer does not have is refused, with a message** (audit
//!    #51): Windows drops such a list without a word when it has more pairs than bands, and sets
//!    whatever it can otherwise; here nothing of it is set, and the command fails naming the bands
//!    that are not there (`crate::commands`). So is a `--set_band_freq` list with a frequency
//!    outside its band's range, a pair Windows drops alone and without a word.
//! 7. **An unknown `--language` is an error** (audit #28): Windows saves any code and shows the
//!    language whose name it starts with, or English. The table's own codes are taken in any case,
//!    and so are the ISO codes it spells otherwise (`uk`, `bs`, `nb`, `nn`) and locales
//!    (`ru_RU.UTF-8`), through [`fxsound_core::i18n::canonical_code`].
//! 8. **Every option works at a start too.** `initConfig` reads ten options and never sees the
//!    band lists, `--set_effect` or the preset management, so a line of them that starts the
//!    Windows build is dropped without a word. Here a start carries them out once the presets are
//!    read ([`Command::honoured_at_cold_start`]). `--next-output` and `--next-input` step from the
//!    device a lane is on, and a start has none yet: a line with one is refused and starts nothing
//!    (`crate::commands::answer_without_an_instance`).
//!
//! Rounding is *not* a departure: `--balance` and `--master_gain` round to the nearest whole
//! number (`FxController.cpp:1803`, `:1815`) and `--filter_q` and `--volume_leveling` to the
//! nearest `0.5` (`FxController.cpp:1827`, `:1791`), matching the step size of the equivalent
//! sliders. The rounding happens in the value parser so a forwarded [`Command`] already carries
//! the value the DSP will see.

use clap::Parser;
use fxsound_core::{
    DeviceDirection, Effect, NoiseSuppressionOverride, ParityClass, ViewMode, WindowsParity, eq,
    i18n,
};

/// The reserved-character set, the length cap and the sanitiser a preset name goes through
/// before it is used (`FxController::sanitizePresetName`, `fxsound/Source/GUI/FxController.cpp:378`).
///
/// They live in `fxsound_preset` beside the store that turns a name into a filename, so that a
/// name saved from the command line and one saved from the window become the same file. 0.3.0
/// kept a second sanitiser here that stripped nine characters while the store replaced three,
/// and a `--save_preset` name and a window-typed name could land in two files.
pub use fxsound_preset::{
    MAX_NAME_BYTES, MAX_PRESET_NAME_LEN, PRESET_NAME_RESERVED, new_preset_name,
    sanitise_preset_name,
};

/// The only band counts the equalizer accepts; anything else is `DEFAULT_NUM_EQ_BANDS = 10` on
/// Windows (`FxController.cpp:283-293`, `FxController.h:45`) and an error here.
pub const VALID_BAND_COUNTS: [u32; 5] = [5, 10, 15, 20, 31];

/// `-20.0..=+20.0` dB, from `DEFAULT_BALANCE`'s validation at `FxController.cpp:307-317`.
pub const BALANCE_RANGE_DB: (f32, f32) = (-20.0, 20.0);
/// `-20.0..=+20.0` dB (`FxController.cpp:331-341`).
pub const MASTER_GAIN_RANGE_DB: (f32, f32) = (-20.0, 20.0);
/// `1.0..=3.0` (`FxController.cpp:319-329`, `DEFAULT_FILTER_Q` at `FxController.h:49`).
pub const FILTER_Q_RANGE: (f32, f32) = (1.0, 3.0);
/// `0.0..=4.0` dB (`FxController.cpp:295-305`, `DEFAULT_VOLUME_LEVELING` at `FxController.h:47`).
pub const VOLUME_LEVELING_RANGE: (f32, f32) = (0.0, 4.0);
/// The effect sliders' own scale, `0.0..=10.0` (`docs/COMMAND_LINE_OPTIONS.md:34`). The DSP stores
/// `0.0..=1.0`; convert with [`fxsound_core::scale::slider_to_value_for`], which spreads Dynamic
/// Boost's and Ambience's positions over the values that sound different.
pub const EFFECT_RANGE: (f32, f32) = (0.0, 10.0);
/// The audible range a band centre may be placed in. The *per band* limit depends on the
/// neighbouring bands and is enforced by `setEqBandFrequency` on the running instance
/// (`docs/COMMAND_LINE_OPTIONS.md:32`), so this is only the outer bound.
pub const BAND_FREQ_RANGE_HZ: (f32, f32) = (20.0, 20_000.0);
/// `--set_effect` never carries more than the five effects (`FxController.cpp:574-600`).
pub const MAX_EFFECT_PAIRS: usize = Effect::COUNT;

/// Everything the command line can say.
///
/// Field order follows `docs/COMMAND_LINE_OPTIONS.md`'s table so the two can be diffed by eye. The
/// hyphenated spellings (`--num-bands`) are hidden aliases of the underscored ones, per
/// `docs/spec/07-startup-tray.md` §4.7.
#[derive(Debug, Clone, Default, Parser)]
#[command(
    name = "fxsound",
    version,
    about = "System-wide audio enhancement: EQ, ambience, surround, bass and dynamic boost",
    long_about = "Run with no options to start FxSound, or to raise the window of an instance \
                  that is already running. Any option given while an instance is running is \
                  forwarded to it over the control socket in $XDG_RUNTIME_DIR/fxsound.",
    // The four reports `--json` can reshape. A group rather than four `requires`, because clap
    // reads a list of `requires` as "all of them".
    group(
        clap::ArgGroup::new("report")
            .args(["status", "watch", "self_test", "list_apps"])
            .multiple(true)
    ),
    // One preset option per line (0.4.0 audit #30): the original takes the first and drops the
    // rest without a word (`FxController.cpp:393-448`), so `--save_preset=A --preset=B` selected B
    // and saved nothing.
    group(
        clap::ArgGroup::new("preset_option")
            .args([
                "preset",
                "save_preset",
                "overwrite_preset",
                "undo_preset",
                "rename_preset",
                "delete_preset",
                "next_preset",
                "prev_preset",
            ])
            .multiple(false)
    )
)]
pub struct Cli {
    /// Turn audio processing on or off. Any non-zero integer means on.
    ///
    /// `--power=<0|1>` — `docs/COMMAND_LINE_OPTIONS.md:17`, `FxController.cpp:241-247`, `:367-373`.
    #[arg(long = "power", value_name = "0|1|toggle", value_parser = parse_power)]
    pub power: Option<PowerArg>,

    /// Select a preset by its exact, case-sensitive name. One preset option per line.
    ///
    /// `--preset=<name>` — `docs/COMMAND_LINE_OPTIONS.md:18`, `FxController.cpp:249-252`, `:395-401`.
    #[arg(long = "preset", value_name = "NAME", value_parser = parse_preset_name)]
    pub preset: Option<String>,

    /// Save the current settings as a new user preset, a copy when nothing is modified.
    ///
    /// `--save_preset=<name>` — `docs/COMMAND_LINE_OPTIONS.md:19`, `FxController.cpp:402-412`; the
    /// original refuses it with nothing modified (0.4.0 audit #17).
    #[arg(
        long = "save_preset",
        alias = "save-preset",
        value_name = "NAME",
        value_parser = parse_new_preset_name
    )]
    pub save_preset: Option<String>,

    /// Overwrite the selected user preset with its unsaved changes.
    ///
    /// `--overwrite_preset` — `docs/COMMAND_LINE_OPTIONS.md:20`, `FxController.cpp:413-420`.
    #[arg(long = "overwrite_preset", alias = "overwrite-preset")]
    pub overwrite_preset: bool,

    /// Discard the selected preset's unsaved changes.
    ///
    /// `--undo_preset` — `docs/COMMAND_LINE_OPTIONS.md:21`, `FxController.cpp:421-427`.
    #[arg(long = "undo_preset", alias = "undo-preset")]
    pub undo_preset: bool,

    /// Rename the selected user preset.
    ///
    /// `--rename_preset=<name>` — `docs/COMMAND_LINE_OPTIONS.md:22`, `FxController.cpp:428-440`.
    #[arg(
        long = "rename_preset",
        alias = "rename-preset",
        value_name = "NAME",
        value_parser = parse_new_preset_name
    )]
    pub rename_preset: Option<String>,

    /// Delete the selected user preset.
    ///
    /// `--delete_preset` — `docs/COMMAND_LINE_OPTIONS.md:23`, `FxController.cpp:441-448`.
    #[arg(long = "delete_preset", alias = "delete-preset")]
    pub delete_preset: bool,

    /// Select the playback device the output lane renders to, by its exact name, or `off` to
    /// detach the lane.
    ///
    /// `--output=<device>` — `docs/COMMAND_LINE_OPTIONS.md:24`, `FxController.cpp:254-257`,
    /// `:451-462`. On Windows this is the WASAPI friendly name; here it is the PipeWire
    /// `node.description`, with `node.name` accepted too since that is what settings persist.
    /// `off` is a Linux addition (0.4.0 design §1.4). In 0.3.0 naming a capture device here
    /// switched FxSound into its input mode; a microphone named here is still selected, for the
    /// input lane, with a note on stderr that `--input` is the option for it now.
    #[arg(long = "output", value_name = "DEVICE|off")]
    pub output: Option<String>,

    /// Select the microphone the input lane reads from, by its exact name, or `off` to detach the
    /// lane.
    ///
    /// Linux addition (0.4.0 design §1.4): the input lane runs beside the output lane, so it has
    /// an option of its own. Matched like `--output`: `node.name` first, then the description.
    #[arg(long = "input", value_name = "DEVICE|off")]
    pub input: Option<String>,

    /// Forget a device that is not connected: drop it from its lane's device priority list, and
    /// the preset remembered for it, as the ✕ beside its row in Settings does.
    ///
    /// Linux addition (0.4.0 audit #34). By `node.name` or description, playback devices and
    /// microphones alike; a device that is connected is refused, since FxSound would learn it
    /// again at once. Not with `--status`, `--watch`, `--self-test` or `--list-apps`: a report
    /// runs alone, and the device would not be forgotten.
    #[arg(
        long = "forget-device",
        alias = "forget_device",
        value_name = "DEVICE",
        value_parser = parse_device_name,
        conflicts_with = "report"
    )]
    pub forget_device: Option<String>,

    /// Which lane the window edits and the preset, effect and level options act on.
    ///
    /// Linux addition (0.4.0 design §1.1): a GUI concern only; the engine has no notion of it.
    /// Picking a device with `--output` or `--input` changes it too, and an `--edit` on the same
    /// line wins over that.
    #[arg(long = "edit", value_name = "output|input", value_parser = parse_direction)]
    pub edit: Option<DeviceDirection>,

    /// Switch the window between the Lite (1) and Pro (2) layouts.
    ///
    /// `--view=<1|2>` — `docs/COMMAND_LINE_OPTIONS.md:25`, `FxController.cpp:259-267`, `:507-516`.
    #[arg(long = "view", value_name = "1|2", value_parser = parse_view)]
    pub view: Option<ViewMode>,

    /// Display language: a code such as `en`, `ru` or `pt-br`, or `system` to follow the desktop.
    ///
    /// `--language=<code>` — `docs/COMMAND_LINE_OPTIONS.md:26`, `FxController.cpp:269-278`. Held
    /// as the table's own code, or `system`: an ISO code or a locale is read into it, and a
    /// language with no table is an error (0.4.0 audit #28).
    #[arg(long = "language", value_name = "CODE|system", value_parser = parse_language)]
    pub language: Option<String>,

    /// «Как в Windows» / "Like FxSound for Windows": `off`, `interface` or `sound`, saved like
    /// the slider in Settings ▸ Experimental.
    ///
    /// Linux addition (`docs/0.5.0-windows-parity.md`). `full` (`everything`) is read, and
    /// refused: Everything arrives in a later version. Once it is offered, a move to `full` that
    /// would switch off something in use — the microphone lane, an application's own preset, a
    /// calibration — is refused unless `--force` is given too: the window asks first, and a
    /// command line cannot.
    #[arg(
        long = "windows-parity",
        alias = "windows_parity",
        value_name = "off|interface|sound",
        value_parser = parse_windows_parity
    )]
    pub windows_parity: Option<WindowsParity>,

    /// With `--windows-parity=full`, from the version that offers it: switch even when it takes
    /// something in use away. Refused with the rest of `full` until then.
    #[arg(long = "force", requires = "windows_parity")]
    pub force: bool,

    /// Number of equalizer bands: 5, 10, 15, 20 or 31.
    ///
    /// `--num_bands=<n>` — `docs/COMMAND_LINE_OPTIONS.md:27`, `FxController.cpp:283-293`, `:468-473`.
    #[arg(long = "num_bands", alias = "num-bands", value_name = "N", value_parser = parse_num_bands)]
    pub num_bands: Option<u32>,

    /// Stereo balance in dB, -20..=20, rounded to a whole number. Positive pans right.
    ///
    /// `--balance=<n>` — `docs/COMMAND_LINE_OPTIONS.md:28`, `FxController.cpp:307-317`, `:484-489`.
    #[arg(
        long = "balance",
        value_name = "DB",
        allow_negative_numbers = true,
        value_parser = parse_balance
    )]
    pub balance: Option<f32>,

    /// Equalizer filter Q, 1.0..=3.0, rounded to the nearest 0.5.
    ///
    /// `--filter_q=<n>` — `docs/COMMAND_LINE_OPTIONS.md:29`, `FxController.cpp:319-329`, `:492-497`.
    #[arg(long = "filter_q", alias = "filter-q", value_name = "Q", value_parser = parse_filter_q)]
    pub filter_q: Option<f32>,

    /// Master gain in dB, -20..=20, rounded to a whole number.
    ///
    /// `--master_gain=<n>` — `docs/COMMAND_LINE_OPTIONS.md:30`, `FxController.cpp:331-341`, `:500-505`.
    #[arg(
        long = "master_gain",
        alias = "master-gain",
        value_name = "DB",
        allow_negative_numbers = true,
        value_parser = parse_master_gain
    )]
    pub master_gain: Option<f32>,

    /// Volume levelling amount, 0.0..=4.0, rounded to the nearest 0.5.
    ///
    /// `--volume_leveling=<n>` — `docs/COMMAND_LINE_OPTIONS.md:31`, `FxController.cpp:295-305`,
    /// `:476-481`. Note the original's single-`l` American spelling; it is kept verbatim.
    #[arg(
        long = "volume_leveling",
        alias = "volume-leveling",
        value_name = "AMOUNT",
        value_parser = parse_volume_leveling
    )]
    pub volume_leveling: Option<f32>,

    /// Noise suppression for the microphone: `off`, `light`, `medium`, `strong`, or `preset` to
    /// follow the voice preset.
    ///
    /// Linux addition (0.4.0 design §2 & 3): the Settings pane's global override, which wins over
    /// every voice preset until it is set back to `preset`. `mild` is accepted for `light`,
    /// because that is what the window calls it.
    #[arg(
        long = "noise-suppression",
        alias = "noise_suppression",
        value_name = "LEVEL",
        value_parser = parse_noise_suppression
    )]
    pub noise_suppression: Option<NoiseSuppressionOverride>,

    /// Give an application a playback preset of its own, `APP=PRESET`, or `APP=default` to have
    /// it follow the output lane's preset again. May be given more than once.
    ///
    /// Linux addition (per-application presets, `docs/0.4.0-apps.md`). `APP` is a remembered
    /// application's Flatpak id, program or name, in any case (`--list-apps` shows them); one
    /// FxSound has not seen gets a rule for the Flatpak id, name or program `APP` looks like
    /// (`crate::app::unseen_key`). `PRESET` is an output preset's exact name; `default` and
    /// `follow` are the lane's.
    #[arg(
        long = "app-preset",
        alias = "app_preset",
        value_name = "APP=PRESET",
        value_parser = parse_app_rule
    )]
    pub app_preset: Vec<AppRuleArg>,

    /// The same for what an application records: a voice preset of its own, or `default`.
    #[arg(
        long = "app-input-preset",
        alias = "app_input_preset",
        value_name = "APP=PRESET",
        value_parser = parse_app_rule
    )]
    pub app_input_preset: Vec<AppRuleArg>,

    /// Band centre frequencies, `band:hz[,band:hz...]`, band index 0-based.
    ///
    /// `--set_band_freq=<…>` — `docs/COMMAND_LINE_OPTIONS.md:32`, `FxController.cpp:536-553`.
    #[arg(
        long = "set_band_freq",
        alias = "set-band-freq",
        value_name = "B:HZ[,B:HZ...]",
        value_parser = parse_band_frequencies
    )]
    pub set_band_freq: Option<BandPairs>,

    /// Band gains in dB, `band:gain[,band:gain...]`, band index 0-based, gain -12..=12.
    ///
    /// `--set_band_gain=<…>` — `docs/COMMAND_LINE_OPTIONS.md:33`, `FxController.cpp:555-572`.
    #[arg(
        long = "set_band_gain",
        alias = "set-band-gain",
        value_name = "B:DB[,B:DB...]",
        allow_negative_numbers = true,
        value_parser = parse_band_gains
    )]
    pub set_band_gain: Option<BandPairs>,

    /// Effect levels, `name:value[,name:value...]`, value 0..=10.
    ///
    /// `--set_effect=<…>` — `docs/COMMAND_LINE_OPTIONS.md:34`, `FxController.cpp:574-600`.
    #[arg(
        long = "set_effect",
        alias = "set-effect",
        value_name = "NAME:V[,NAME:V...]",
        value_parser = parse_effects
    )]
    pub set_effect: Option<EffectPairs>,

    /// Print the running instance's state and exit. Every other option is ignored.
    ///
    /// One `key: value` line per item, which is what scripts already grep; pass `--json`
    /// alongside for a single JSON object instead.
    ///
    /// `--status` — `docs/COMMAND_LINE_OPTIONS.md:35`, `FxController.cpp:348-352`, `:635-700`.
    /// Unlike Windows, the answer comes back over the control socket and is printed on *this*
    /// process's stdout. Earlier versions of this help promised JSON by default and a
    /// `$XDG_RUNTIME_DIR/fxsound/status.json` written as a courtesy; neither was ever true. The
    /// JSON is now real and explicit, and the file is not written at all — a status file that is
    /// only refreshed when somebody asks for it is stale by definition, and a status bar is
    /// better served by running this command than by reading a file nothing keeps current.
    #[arg(long = "status")]
    pub status: bool,

    /// Print the `--status`, `--watch`, `--self-test` or `--list-apps` report as JSON rather
    /// than as lines.
    ///
    /// Deliberately not the default: the line format is what every existing script greps, and
    /// changing it under them would be a breaking change to buy a format they did not ask for.
    /// Given without any of the four it is an error, not a no-op.
    #[arg(long = "json", requires = "report")]
    pub json: bool,

    /// Print the applications FxSound remembers — the ones playing or recording now first — with
    /// the preset each has of its own and the one it runs through now, and exit.
    ///
    /// Linux addition (per-application presets, `docs/0.4.0-apps.md`). A question like
    /// `--status`: every other option on the line is ignored and the window is left alone. With
    /// no FxSound running it answers from the store, `apps.toml`, in which nothing is running.
    #[arg(long = "list-apps", alias = "list_apps")]
    pub list_apps: bool,

    /// Subscribe to the running instance and print one event per line until it quits.
    ///
    /// Linux addition (0.4.0 design §10). The first event is the full `--status` document. Like
    /// `--status`, every other option on the line is ignored and the window is left alone.
    #[arg(long = "watch")]
    pub watch: bool,

    /// With `--watch`, also stream the microphone's meters, at most four times a second.
    #[arg(long = "meters", requires = "watch")]
    pub meters: bool,

    /// Check the installation and exit: presets, settings, both processing chains offline, the
    /// installed files, and whether PipeWire and a session bus are there to be used.
    ///
    /// Linux addition (0.4.0 design §13). It runs in this process before the single-instance
    /// lock is taken, so it works beside a running FxSound and in a bare container, and it
    /// never forwards anything. Every other option on the line is ignored.
    #[arg(long = "self-test", alias = "self_test")]
    pub self_test: bool,

    /// Start without showing the window, or hide the window of a running instance.
    ///
    /// `--run_minimized` — `docs/COMMAND_LINE_OPTIONS.md:36`, `FxController.cpp:236-239`,
    /// `:523-531`. `--hide` is the Linux spelling and is what `packaging/fxsound-autostart.desktop`
    /// and `packaging/fxsound.service` invoke.
    #[arg(
        long = "run_minimized",
        alias = "run-minimized",
        visible_alias = "hide"
    )]
    pub run_minimized: bool,

    /// How the D-Bus activation file and the systemd user unit start FxSound: as `--hide`, except
    /// that when FxSound is already running this process exits at once and forwards nothing.
    ///
    /// Linux addition. The bus starts FxSound for a call to a name nobody owns yet, and an
    /// instance started from the launcher only owns it once its audio engine is up: a status
    /// bar's poll landing in between used to start `fxsound --hide`, which found the lock taken,
    /// forwarded `--hide` and hid the window that had just been opened.
    #[arg(long = "activated")]
    pub activated: bool,

    /// Show and raise the window of the running instance, or start with it showing.
    ///
    /// Linux addition (`docs/spec/07-startup-tray.md` §4.7). The original has no such option
    /// because *every* forwarded command line except `--status` raises the window; here only
    /// this, `--view` and a line with no options do (0.4.0 audit R11, `Cli::window_command`).
    #[arg(long = "show")]
    pub show: bool,

    /// Toggle audio processing. Compositor stand-in for `CMD_ON_OFF`, Ctrl+Shift+Q
    /// (`FxController.cpp:2820-2866`, defaults at `Settings.cpp:34`).
    #[arg(long = "toggle-power", alias = "toggle_power")]
    pub toggle_power: bool,

    /// Show the window if it is hidden, hide it if it is showing. Compositor stand-in for
    /// `CMD_OPEN_CLOSE`, Ctrl+Shift+E (`Settings.cpp:35`), and the same toggle a left click on the
    /// tray icon performs (`FxSystemTrayView.cpp:446-455`).
    #[arg(long = "toggle-window", alias = "toggle_window")]
    pub toggle_window: bool,

    /// Select the next preset, wrapping. Compositor stand-in for `CMD_NEXT_PRESET`, Ctrl+Shift+A
    /// (`Settings.cpp:36`, behaviour at `FxController.cpp:1923-2014`).
    #[arg(long = "next-preset", alias = "next_preset")]
    pub next_preset: bool,

    /// Select the previous preset, wrapping. Compositor stand-in for `CMD_PREVIOUS_PRESET`,
    /// Ctrl+Shift+Z (`Settings.cpp:37`).
    #[arg(long = "prev-preset", aliases = ["prev_preset", "previous-preset", "previous_preset"])]
    pub prev_preset: bool,

    /// Cycle the output lane to the next playback device, wrapping; never into the microphones.
    /// Compositor stand-in for `CMD_NEXT_OUTPUT`, Ctrl+Shift+W (`Settings.cpp:38`).
    #[arg(long = "next-output", alias = "next_output")]
    pub next_output: bool,

    /// Cycle the input lane to the next microphone, wrapping. The input lane's `--next-output`.
    #[arg(long = "next-input", alias = "next_input")]
    pub next_input: bool,

    /// Quit the running instance, autosaving a modified preset first.
    ///
    /// Linux addition (`docs/spec/07-startup-tray.md` §4.7). On Windows the *only* way to quit is
    /// the tray's Exit item (`FxSystemTrayView.cpp:285-287`); with no tray host available on some
    /// desktops that would leave the app unkillable by UI.
    #[arg(long = "quit")]
    pub quit: bool,
}

/// The value of `--power`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerArg {
    /// `--power=1`, or any other non-zero integer, or `on`.
    On,
    /// `--power=0`, or `off`.
    Off,
    /// `--power=toggle`. Linux addition (`docs/spec/07-startup-tray.md` §4.7).
    Toggle,
}

/// A parsed `band:value` list, newtyped so `clap` treats the whole list as one value rather than
/// as a repeated argument.
#[derive(Debug, Clone, PartialEq)]
pub struct BandPairs(pub Vec<(usize, f32)>);

/// A parsed `effect:value` list. Values are on the CLI's `0..=10` scale.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectPairs(pub Vec<(Effect, f32)>);

/// One `APP=PRESET` of `--app-preset` or `--app-input-preset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRuleArg {
    /// The application, as the user wrote it (trimmed): a Flatpak id, a program or a name.
    pub app: String,
    /// What it is to run through.
    pub preset: AppPresetChoice,
}

/// What an application runs through in one direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppPresetChoice {
    /// The lane's own preset, whatever it is: `default`, `follow`, or nothing after the `=`.
    Follow,
    /// A preset of its own, by the exact name the lane's preset list has.
    Preset(String),
}

impl AppPresetChoice {
    /// Read a preset argument: `default`, `follow` (in any case) or nothing is
    /// [`AppPresetChoice::Follow`], anything else the preset of that name. No shipped preset is
    /// called either word; a user preset that is has to be renamed to be given to an application.
    /// D-Bus's `SetAppPreset` reads its argument with it too.
    #[must_use]
    pub fn parse(preset: &str) -> Self {
        let preset = preset.trim();
        let follows = preset.is_empty()
            || preset.eq_ignore_ascii_case("default")
            || preset.eq_ignore_ascii_case("follow");
        if follows {
            Self::Follow
        } else {
            Self::Preset(preset.to_owned())
        }
    }

    /// The preset's name, `None` to follow the lane: what [`crate::App::set_app_preset`] takes.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Follow => None,
            Self::Preset(name) => Some(name),
        }
    }
}

/// One thing to do to the application, in the order `applyConfig` does it.
///
/// This is what crosses the single-instance socket (as re-parsed argv — see [`crate::ipc`]) and
/// what a tray or hotkey path funnels into. It is deliberately *not* the full controller API: it
/// carries only what a command line can express.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Report state and do nothing else (`FxController.cpp:348-352`).
    Status {
        /// One JSON object rather than one line per item.
        json: bool,
    },
    /// Subscribe to the event stream and do nothing else (0.4.0 design §10).
    Watch {
        /// One JSON object per event rather than `event key=value` lines.
        json: bool,
        /// Stream the microphone's meters too.
        meters: bool,
    },
    /// Check the installation and do nothing else (0.4.0 design §13). `main` runs it before the
    /// single-instance lock; it reaches [`crate::commands::run`] only if a running instance is
    /// handed one directly.
    SelfTest {
        /// One JSON document rather than one line per check.
        json: bool,
    },
    /// List the remembered applications and do nothing else (`docs/0.4.0-apps.md`). With no
    /// instance running `main` answers it from the store before the engine starts.
    ListApps {
        /// One JSON object rather than one line per application.
        json: bool,
    },
    Power(PowerCommand),
    Preset(PresetCommand),
    Output(OutputCommand),
    Input(InputCommand),
    /// Drop a device that is not connected from its lane's priority list (`--forget-device`), by
    /// `node.name` or description, in either direction.
    ForgetDevice(String),
    /// The lane the window edits and the preset and level commands act on.
    EditDirection(DeviceDirection),
    /// The microphone's global noise-suppression override.
    NoiseSuppression(NoiseSuppressionOverride),
    /// The preset an application runs through in one direction (`--app-preset`,
    /// `--app-input-preset`). `app` is the text the user named it by; the store resolves it.
    AppPreset {
        direction: DeviceDirection,
        app: String,
        preset: AppPresetChoice,
    },
    NumBands(u32),
    VolumeLeveling(f32),
    Balance(f32),
    FilterQ(f32),
    MasterGain(f32),
    View(ViewMode),
    Language(String),
    /// «Как в Windows» / "Like FxSound for Windows" (`--windows-parity`). `force` goes ahead with
    /// a move to Everything that would take something in use away, which is refused without it,
    /// from the version that offers Everything; this one refuses Everything either way.
    WindowsParity {
        level: WindowsParity,
        force: bool,
    },
    Window(WindowCommand),
    /// `(band index, Hz)` pairs. The caller refuses the whole list, naming the bands, when one of
    /// them is past the live band count (0.4.0 audit #51) — that count is not knowable here.
    BandFrequencies(Vec<(usize, f32)>),
    /// `(band index, dB)` pairs, refused whole the same way as [`Command::BandFrequencies`].
    BandGains(Vec<(usize, f32)>),
    /// `(effect, 0..=10)` pairs.
    Effects(Vec<(Effect, f32)>),
    /// Shut down gracefully, as the tray's Exit item does (`FxController.cpp:996-1005`).
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerCommand {
    On,
    Off,
    Toggle,
}

/// The six preset commands, plus the two the compositor drives.
///
/// One per invocation. `docs/COMMAND_LINE_OPTIONS.md:40` processes the first in the order the
/// `if`/`else if` chain at `FxController.cpp:395-448` tests them and silently drops the rest; here
/// the parser refuses a line with two (0.4.0 audit #30), so there is never a rest to drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetCommand {
    Select(String),
    /// Name already run through [`new_preset_name`] — [`sanitise_preset_name`] and the 126 bytes a
    /// Windows FxSound reads a name in (0.4.0 audit #15); the case-insensitive collision check
    /// against existing names (step 3 of `FxController::sanitizePresetName`, `:385`) still has to
    /// happen where the preset list lives.
    SaveAs(String),
    Overwrite,
    Undo,
    /// Name already sanitised, as for [`PresetCommand::SaveAs`].
    Rename(String),
    Delete,
    Next,
    Previous,
}

/// What to do to one lane's device. The same three for both lanes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceCommand {
    /// Attach the lane to the device with this `node.name` or description.
    Select(String),
    /// Cycle to the lane's next device, wrapping.
    Next,
    /// Detach the lane: `--output off`, `--input off`.
    Detach,
}

/// `--output` and `--next-output`.
pub type OutputCommand = DeviceCommand;
/// `--input` and `--next-input`.
pub type InputCommand = DeviceCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCommand {
    Show,
    Hide,
    Toggle,
}

impl From<PowerArg> for PowerCommand {
    fn from(arg: PowerArg) -> Self {
        match arg {
            PowerArg::On => Self::On,
            PowerArg::Off => Self::Off,
            PowerArg::Toggle => Self::Toggle,
        }
    }
}

impl Cli {
    /// Parse this process's own command line, exiting with `clap`'s message on a bad one.
    ///
    /// The parse happens *before* the single-instance probe so that `fxsound --nonsense` reports
    /// the problem instead of waking a running instance to reject it.
    #[must_use]
    pub fn parse_args() -> Self {
        Self::parse()
    }

    /// The commands this invocation asks a *running* instance to perform, in `applyConfig` order
    /// (`FxController.cpp:344-602`, laid out step by step in `docs/spec/07-startup-tray.md` §4.4).
    ///
    /// Two orderings in there are load-bearing and are reproduced exactly:
    ///
    /// * `--status` returns immediately, so nothing else on the line runs and the window is *not*
    ///   raised (`:348-352`). `--self-test`, `--list-apps` and `--watch` are questions of the same
    ///   kind and do the same; with more than one on a line, `--self-test` wins, then `--status`,
    ///   then `--list-apps`: a question answered once wins over a stream.
    /// * the band and effect lists come *after* `--num_bands`, so
    ///   `--num_bands=31 --set_band_gain="30:6"` works in a single invocation (`:558`).
    ///
    /// One is deliberately not: the devices come *before* the preset, where `applyConfig` selects
    /// the preset first (`:395` before `:451`). With a lane per direction, a device is how a line
    /// says which lane it means — `--input=Headset --preset='Gaming Headset'` has to find the
    /// preset in the microphone's list — and a device brings back the preset remembered for it,
    /// which in the original order silently replaced the preset the same line had just asked for.
    /// `--edit` sits between the two, so that it wins over the lane a device picked. The
    /// applications' presets come right after the preset, so a preset `--save_preset` makes is
    /// one `--app-preset` on the same line can give.
    #[must_use]
    pub fn commands(&self) -> Vec<Command> {
        if self.self_test {
            return vec![Command::SelfTest { json: self.json }];
        }
        if self.status {
            return vec![Command::Status { json: self.json }];
        }
        if self.list_apps {
            return vec![Command::ListApps { json: self.json }];
        }
        if self.watch {
            return vec![Command::Watch {
                json: self.json,
                meters: self.meters,
            }];
        }

        let mut commands = self.commands_before_the_window();
        if let Some(window) = self.window_command() {
            commands.push(Command::Window(window));
        }
        commands.extend(self.commands_after_the_window());
        commands
    }

    /// What the line sets before step 11 of `applyConfig` shows the window: the power, the
    /// devices, the preset, the levels, the view and the language, in [`Cli::commands`]' order.
    fn commands_before_the_window(&self) -> Vec<Command> {
        let mut commands = Vec::new();
        // First, so that everything after it on the line is done at the level it asks for.
        if let Some(level) = self.windows_parity {
            commands.push(Command::WindowsParity {
                level,
                force: self.force,
            });
        }
        if let Some(power) = self.power_command() {
            commands.push(Command::Power(power));
        }
        if let Some(output) = self.output_command() {
            commands.push(Command::Output(output));
        }
        if let Some(input) = self.input_command() {
            commands.push(Command::Input(input));
        }
        if let Some(device) = &self.forget_device {
            commands.push(Command::ForgetDevice(device.clone()));
        }
        if let Some(direction) = self.edit {
            commands.push(Command::EditDirection(direction));
        }
        if let Some(preset) = self.preset_command() {
            commands.push(Command::Preset(preset));
        }
        for (direction, rules) in [
            (DeviceDirection::Output, &self.app_preset),
            (DeviceDirection::Input, &self.app_input_preset),
        ] {
            commands.extend(rules.iter().map(|rule| Command::AppPreset {
                direction,
                app: rule.app.clone(),
                preset: rule.preset.clone(),
            }));
        }
        if let Some(choice) = self.noise_suppression {
            commands.push(Command::NoiseSuppression(choice));
        }
        if let Some(bands) = self.num_bands {
            commands.push(Command::NumBands(bands));
        }
        if let Some(value) = self.volume_leveling {
            commands.push(Command::VolumeLeveling(value));
        }
        if let Some(value) = self.balance {
            commands.push(Command::Balance(value));
        }
        if let Some(value) = self.filter_q {
            commands.push(Command::FilterQ(value));
        }
        if let Some(value) = self.master_gain {
            commands.push(Command::MasterGain(value));
        }
        if let Some(view) = self.view {
            commands.push(Command::View(view));
        }
        if let Some(language) = &self.language {
            commands.push(Command::Language(language.clone()));
        }
        commands
    }

    /// What the line sets after the window step: the band and effect lists, which the original
    /// applies last (`FxController.cpp:536-600`), and the quit.
    fn commands_after_the_window(&self) -> Vec<Command> {
        let mut commands = Vec::new();
        if let Some(pairs) = &self.set_band_freq {
            commands.push(Command::BandFrequencies(pairs.0.clone()));
        }
        if let Some(pairs) = &self.set_band_gain {
            commands.push(Command::BandGains(pairs.0.clone()));
        }
        if let Some(pairs) = &self.set_effect {
            commands.push(Command::Effects(pairs.0.clone()));
        }
        if self.quit {
            commands.push(Command::Quit);
        }
        commands
    }

    /// [`Cli::commands`] as D-Bus `Apply` hands them over: without the window raise a typed
    /// command line gets from `--view` or from having no options at all (0.4.0 design §9, and
    /// `Cli::window_command`). A bus call comes from a keybind or a status bar, as every other
    /// method's does; `--show`, `--toggle-window` and `--hide` on the line still do what they say.
    #[must_use]
    pub fn commands_without_implicit_raise(&self) -> Vec<Command> {
        let mut commands = self.commands();
        if !self.show && self.window_command() == Some(WindowCommand::Show) {
            commands.retain(|command| *command != Command::Window(WindowCommand::Show));
        }
        commands
    }

    /// The subset of [`Cli::commands`] a *cold start* carries out, in step 4 of `main`, once the
    /// presets are read ([`Command::honoured_at_cold_start`]).
    ///
    /// The C column of `docs/spec/07-startup-tray.md` §4.2 is narrower: `initConfig` never sees
    /// the preset-management commands, `--set_band_*` or `--set_effect`, because at that point in
    /// its startup there is no preset list, no audio and no window (`Main.cpp:67` runs it two
    /// lines before `AudioPassthru` exists), and it drops them without a word. Here the list is
    /// there by step 4, so they are carried out as a running instance carries them out.
    ///
    /// An explicit `--show` is kept: it is how a start overrides the remembered "start hidden"
    /// (`run_minimized`, written again since 0.4.0 audit #35), as `--hide` overrides the other way.
    /// The raise of a bare `fxsound` or of `--view` is not (see [`Command::honoured_at_cold_start`]).
    #[must_use]
    pub fn cold_start_commands(&self) -> Vec<Command> {
        self.commands()
            .into_iter()
            .filter(|command| {
                command.honoured_at_cold_start()
                    || (self.show && *command == Command::Window(WindowCommand::Show))
            })
            .collect()
    }

    /// `true` for a line that sets something and says nothing about the window: no `--show`,
    /// `--view`, `--hide`, `--toggle-window` or `--run_minimized`, no question, and not a bare
    /// `fxsound` ([`Cli::window_command`] is `None` for it). A running instance leaves its window
    /// alone for such a line (0.4.0 audit R11), and a cold start from one starts in the tray
    /// rather than putting a window over the game whose keybinding ran it (review FA).
    #[must_use]
    pub fn only_sets_things(&self) -> bool {
        !self.is_query() && self.window_command().is_none()
    }

    /// `true` when this invocation only asks for state and must not disturb the window.
    #[must_use]
    pub const fn is_status(&self) -> bool {
        self.status
    }

    /// `true` when this invocation is a question — `--status`, `--watch`, `--self-test` or
    /// `--list-apps` — rather than something to do, so the window is left alone.
    #[must_use]
    pub const fn is_query(&self) -> bool {
        self.status || self.watch || self.self_test || self.list_apps
    }

    fn power_command(&self) -> Option<PowerCommand> {
        match self.power {
            // An explicit `--power` wins over `--toggle-power`: it is the original's option and it
            // says what it wants, where the toggle only says "change it".
            Some(arg) => Some(arg.into()),
            None if self.toggle_power => Some(PowerCommand::Toggle),
            None => None,
        }
    }

    /// The one preset option on the line, if there is one: the `preset_option` group lets the
    /// parser through with at most one (0.4.0 audit #30), and the name parsers refuse a name that
    /// is empty or sanitises away, so every branch here that matches makes a command.
    fn preset_command(&self) -> Option<PresetCommand> {
        if let Some(name) = &self.preset {
            Some(PresetCommand::Select(name.clone()))
        } else if let Some(name) = &self.save_preset {
            Some(PresetCommand::SaveAs(new_preset_name(name)))
        } else if self.overwrite_preset {
            Some(PresetCommand::Overwrite)
        } else if self.undo_preset {
            Some(PresetCommand::Undo)
        } else if let Some(name) = &self.rename_preset {
            Some(PresetCommand::Rename(new_preset_name(name)))
        } else if self.delete_preset {
            Some(PresetCommand::Delete)
        } else if self.next_preset {
            Some(PresetCommand::Next)
        } else if self.prev_preset {
            Some(PresetCommand::Previous)
        } else {
            None
        }
    }

    fn output_command(&self) -> Option<OutputCommand> {
        device_command(self.output.as_deref(), self.next_output)
    }

    fn input_command(&self) -> Option<InputCommand> {
        device_command(self.input.as_deref(), self.next_input)
    }

    /// Step 11 of `applyConfig` (`FxController.cpp:523-531`), narrowed to the options that are
    /// about the window (0.4.0 audit R11).
    ///
    /// On Windows that step is an `else`: *any* forwarded command line that is not `--status`
    /// shows and raises the window, so `fxsound --preset=Gaming` pops the UI to the front. On
    /// Linux the same CLI is the replacement for the five global hotkeys
    /// (`docs/spec/07-startup-tray.md` §2.7) and what a script or a game's launcher runs, and a
    /// hotkey on Windows goes through `eventCallback` (`:1923-2014`) and never raises anything.
    /// So only what asks for the window gets it: `--show`; `--view`, which switches the layout
    /// the window shows; and a line with no options at all, which is how a desktop launcher
    /// starts FxSound and so how a second launch brings the running one's window up. Every
    /// option that sets something does so silently, and a question (`--status` and the like)
    /// never touches the window.
    fn window_command(&self) -> Option<WindowCommand> {
        if self.is_query() {
            None
        } else if self.run_minimized || self.activated {
            Some(WindowCommand::Hide)
        } else if self.toggle_window {
            Some(WindowCommand::Toggle)
        } else if self.show || self.view.is_some() || self.asks_for_nothing() {
            Some(WindowCommand::Show)
        } else {
            None
        }
    }

    /// `true` for a line with no options — `fxsound`, as a desktop entry runs it — or with none
    /// that makes a command (`--output=`, an empty name, is none).
    fn asks_for_nothing(&self) -> bool {
        self.commands_before_the_window().is_empty() && self.commands_after_the_window().is_empty()
    }
}

impl Command {
    /// Whether a cold start carries this command out, in step 4 of `main`.
    ///
    /// Every command that sets something is carried out, where `initConfig` acts only on the ten
    /// of `docs/spec/07-startup-tray.md` §4.3 and drops the band lists, `--set_effect` and the
    /// preset management without a word. Not on the list: what a start has nothing for. The
    /// questions are answered by `main` before the engine starts, `--forget-device` is done to the
    /// settings file there, and `--next-output` / `--next-input` step from the device a lane is on,
    /// which a start does not have yet, so `main` refuses a line with one
    /// (`crate::commands::answer_without_an_instance`). [`WindowCommand::Show`] is
    /// pointedly *not* on it: a plain `fxsound` emits one (see `Cli::window_command`), and
    /// honouring it at startup would override the persisted `run_minimized` on every launch and
    /// break "quit with the window hidden, start hidden next time" (§7.1). A cold start's
    /// visibility comes from the setting, with `--run_minimized` and an explicit `--show` as the
    /// overrides ([`Cli::cold_start_commands`] keeps the latter).
    ///
    /// [`Command::Quit`] is not on it either: a cold start is the proof that there is nothing
    /// to quit, and `main` says so before the audio engine starts rather than starting an
    /// instance for this list to then stop.
    #[must_use]
    pub fn honoured_at_cold_start(&self) -> bool {
        // Exhaustive on purpose: a new command has to say which half of §4.2's C/R split it is in.
        match self {
            Self::Power(_)
            | Self::NumBands(_)
            | Self::VolumeLeveling(_)
            | Self::Balance(_)
            | Self::FilterQ(_)
            | Self::MasterGain(_)
            | Self::View(_)
            | Self::Language(_) => true,
            // Saved like `--language`; at a start nothing is in use yet for a move to Everything
            // to take away.
            Self::WindowsParity { .. } => true,
            // Past §4.3: the presets are read by step 4, so the band lists, the effects and every
            // preset command work on the preset the start has selected, as they would a moment
            // later on the running instance — where `initConfig` drops them without a word.
            Self::BandFrequencies(_) | Self::BandGains(_) | Self::Effects(_) | Self::Preset(_) => {
                true
            }
            // `--output` is on §4.3's list, `--input` is its twin, and `off` is a choice of
            // device like any other. Cycling steps from the device a lane is on, and a start has
            // none yet: `main` refuses the line before anything starts.
            Self::Output(device) | Self::Input(device) => {
                matches!(device, DeviceCommand::Select(_) | DeviceCommand::Detach)
            }
            // Done by `main` to the settings file before anything starts
            // (`commands::forget_in_the_settings_file`): run in step 4, before PipeWire has
            // listed a device, it could only be refused, and a line of its own starts nothing.
            Self::ForgetDevice(_) => false,
            // Settings a start-up line may well carry, like `--view` and `--balance`. An
            // application's preset is one too, and like `--preset` it is chosen once the preset
            // lists are read, so a name neither list has is still refused, on stderr.
            Self::EditDirection(_) | Self::NoiseSuppression(_) | Self::AppPreset { .. } => true,
            // A toggle of a window that is not there yet brings it up: `main` reads it as `--show`.
            Self::Window(window) => matches!(window, WindowCommand::Hide | WindowCommand::Toggle),
            // Questions for a running instance, which a cold start proves there is not; `main`
            // answers all four before the engine starts — `--list-apps` from the store.
            Self::Status { .. }
            | Self::Watch { .. }
            | Self::SelfTest { .. }
            | Self::ListApps { .. }
            | Self::Quit => false,
        }
    }
}

impl Command {
    /// The level of «Как в Windows» at which this command stops doing what it does in FxSound for
    /// Linux (`docs/0.5.0-windows-parity.md`, "Classification").
    ///
    /// Exhaustive on purpose, with no `_` arm: a new command does not compile until someone has
    /// decided its level.
    ///
    /// - [`ParityClass::Interface`]: the options the Windows build has
    ///   (`docs/COMMAND_LINE_OPTIONS.md`), whose window behaviour (0.4.0 audit R11) and band lists
    ///   (#51) go back to Windows there, and the preset commands whose availability does (#17,
    ///   #18).
    /// - [`ParityClass::Full`]: what only this port has and Everything hides — the microphone
    ///   lane, the applications' presets.
    /// - [`ParityClass::Never`]: the questions, the keybind options that stand in for the Windows
    ///   hotkeys, the window, forgetting a device, quitting, and this option itself, which is the
    ///   way back.
    #[must_use]
    pub const fn parity_class(&self) -> ParityClass {
        match self {
            Self::Status { .. } | Self::Watch { .. } | Self::SelfTest { .. } => ParityClass::Never,
            // Answers with the applications hidden at Everything rather than refusing, as D-Bus's
            // `ListApps` does.
            Self::ListApps { .. } => ParityClass::Full,
            Self::Power(power) => match power {
                PowerCommand::On | PowerCommand::Off => ParityClass::Interface,
                // `--toggle-power` is the stand-in for the Windows hotkey, which never raised
                // the window.
                PowerCommand::Toggle => ParityClass::Never,
            },
            Self::Preset(preset) => match preset {
                PresetCommand::Select(_)
                | PresetCommand::SaveAs(_)
                | PresetCommand::Overwrite
                | PresetCommand::Undo
                | PresetCommand::Rename(_)
                | PresetCommand::Delete => ParityClass::Interface,
                PresetCommand::Next | PresetCommand::Previous => ParityClass::Never,
            },
            Self::Output(device) => match device {
                DeviceCommand::Select(_) => ParityClass::Interface,
                // `--next-output` is a hotkey's stand-in; `--output=off` is the port's own.
                DeviceCommand::Next | DeviceCommand::Detach => ParityClass::Never,
            },
            Self::Input(_)
            | Self::EditDirection(_)
            | Self::NoiseSuppression(_)
            | Self::AppPreset { .. } => ParityClass::Full,
            Self::ForgetDevice(_) => ParityClass::Never,
            Self::NumBands(_)
            | Self::VolumeLeveling(_)
            | Self::Balance(_)
            | Self::FilterQ(_)
            | Self::MasterGain(_)
            | Self::View(_)
            | Self::Language(_)
            | Self::BandFrequencies(_)
            | Self::BandGains(_)
            | Self::Effects(_) => ParityClass::Interface,
            Self::WindowsParity { .. } | Self::Window(_) | Self::Quit => ParityClass::Never,
        }
    }
}

/// `--output`/`--input` and their `--next-*`: a name wins over cycling, as it always has for
/// `--output`, and an empty name is no command at all, as `applyConfig` treats an empty
/// `--output`. D-Bus's `SetOutput` and `SetInput` read their argument with it too.
pub(crate) fn device_command(name: Option<&str>, next: bool) -> Option<DeviceCommand> {
    match name {
        Some(name) if name.trim().eq_ignore_ascii_case("off") => Some(DeviceCommand::Detach),
        Some(name) => (!name.is_empty()).then(|| DeviceCommand::Select(name.to_owned())),
        None => next.then_some(DeviceCommand::Next),
    }
}

/// `--edit`'s value; D-Bus's `SetEditDirection` reads its argument with it too (`crate::dbus`).
pub(crate) fn parse_direction(value: &str) -> Result<DeviceDirection, String> {
    DeviceDirection::from_key(&value.trim().to_ascii_lowercase())
        .ok_or_else(|| format!("expected output or input, got `{value}`"))
}

/// `--app-preset`'s and `--app-input-preset`'s value: `APP=PRESET`, split at the first `=` — a
/// preset's name may hold one, a program's or a Flatpak id's never does. `APP` cannot be empty;
/// `PRESET` is read by [`AppPresetChoice::parse`].
fn parse_app_rule(value: &str) -> Result<AppRuleArg, String> {
    let (app, preset) = value
        .split_once('=')
        .ok_or_else(|| format!("expected APP=PRESET, got `{value}`"))?;
    let app = app.trim();
    if app.is_empty() {
        return Err(format!(
            "expected APP=PRESET with an application before the `=`, got `{value}`"
        ));
    }
    Ok(AppRuleArg {
        app: app.to_owned(),
        preset: AppPresetChoice::parse(preset),
    })
}

/// `--preset`'s value: a name, which has to be there. 0.3.0 and the original took `--preset=` as no
/// command at all and exited 0 (`FxController.cpp:397`); D-Bus's `SetPreset` already refused it.
pub(crate) fn parse_preset_name(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("a preset name cannot be empty".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

/// `--save_preset`'s and `--rename_preset`'s value, as typed: refused when nothing is left of it
/// once [`new_preset_name`] has taken out the characters a preset name cannot hold — `::` saved
/// nothing and said nothing before (0.4.0 audit #30). The name the command carries is sanitised
/// again when it is built ([`Cli::commands`]), so what the parser keeps is what the user wrote.
fn parse_new_preset_name(value: &str) -> Result<String, String> {
    if new_preset_name(value).is_empty() {
        let reserved: String = PRESET_NAME_RESERVED.iter().collect();
        // A line break or a tab is shown as its escape, not as itself.
        let shown: String = value
            .chars()
            .map(|c| {
                if c.is_control() {
                    c.escape_default().to_string()
                } else {
                    c.to_string()
                }
            })
            .collect();
        Err(format!(
            "`{shown}` is no preset name: nothing is left of it once the characters a preset \
             name cannot hold ({reserved}), control characters such as a line break and the \
             spaces around them are taken out"
        ))
    } else {
        Ok(value.to_owned())
    }
}

/// `--forget-device`'s value: a `node.name` or a description, which has to be there.
fn parse_device_name(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("a device name cannot be empty".to_owned())
    } else {
        Ok(value.to_owned())
    }
}

/// `--windows-parity`'s value: `off`, `interface`, `sound` or `full` in any case, and `everything`
/// for `full`. D-Bus's `SetWindowsParity` reads its argument with it too.
///
/// `full` is read, so that [`crate::app::App::set_windows_parity`] refuses it with the one text
/// every path shares ([`fxsound_core::parity::FULL_NOT_YET`]) rather than as an unknown word.
pub(crate) fn parse_windows_parity(value: &str) -> Result<WindowsParity, String> {
    WindowsParity::parse(value)
        .ok_or_else(|| format!("expected off, interface or sound, got `{value}`"))
}

/// `--language`'s value: `system` (also `default`) to follow the desktop session, or a language
/// FxSound has a table for, as [`i18n::canonical_code`] reads one — its code in any case, the ISO
/// code where the Windows build spells it otherwise (`uk`, `bs`, `nb`, `nn`), or a locale such as
/// `ru_RU.UTF-8`. Comes back as the table's own code, or `system`. Anything else is an error that
/// lists the codes: 0.3.0 saved it and showed the system's language instead (0.4.0 audit #28).
fn parse_language(value: &str) -> Result<String, String> {
    let text = value.trim();
    if text.eq_ignore_ascii_case("system") || text.eq_ignore_ascii_case("default") {
        return Ok("system".to_owned());
    }
    i18n::canonical_code(text)
        .map(str::to_owned)
        .ok_or_else(|| {
            let mut codes: Vec<&str> = i18n::LANGUAGES.iter().map(|l| l.code).collect();
            codes.sort_unstable_by_key(|code| code.to_ascii_lowercase());
            format!(
                "FxSound has no translation for `{value}`; expected system or one of {}",
                codes.join(", ")
            )
        })
}

/// `--noise-suppression`'s value; D-Bus's `SetNoiseSuppression` reads its argument with it too.
pub(crate) fn parse_noise_suppression(value: &str) -> Result<NoiseSuppressionOverride, String> {
    let key = value.trim().to_ascii_lowercase();
    // The window says "Mild"; the key says `light` (see `DenoiseLevel::label`).
    let key = if key == "mild" { "light" } else { key.as_str() };
    NoiseSuppressionOverride::from_key(key)
        .ok_or_else(|| format!("expected preset, off, light, medium or strong, got `{value}`"))
}

fn parse_power(value: &str) -> Result<PowerArg, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "toggle" => Ok(PowerArg::Toggle),
        "on" => Ok(PowerArg::On),
        "off" => Ok(PowerArg::Off),
        // `FxController.cpp:241-247` reads an int and tests it against zero, so `--power=2` is on.
        other => match other.parse::<i64>() {
            Ok(0) => Ok(PowerArg::Off),
            Ok(_) => Ok(PowerArg::On),
            Err(_) => Err(format!("expected 0, 1 or toggle, got `{value}`")),
        },
    }
}

fn parse_view(value: &str) -> Result<ViewMode, String> {
    match value.trim() {
        "1" => Ok(ViewMode::Lite),
        "2" => Ok(ViewMode::Pro),
        other => Err(format!("expected 1 (Lite) or 2 (Pro), got `{other}`")),
    }
}

fn parse_num_bands(value: &str) -> Result<u32, String> {
    let bands: u32 = value
        .trim()
        .parse()
        .map_err(|_| format!("expected a whole number, got `{value}`"))?;
    if VALID_BAND_COUNTS.contains(&bands) {
        Ok(bands)
    } else {
        Err(format!(
            "expected one of 5, 10, 15, 20 or 31 bands, got {bands}"
        ))
    }
}

fn parse_balance(value: &str) -> Result<f32, String> {
    parse_ranged(value, BALANCE_RANGE_DB, "dB").map(f32::round)
}

fn parse_master_gain(value: &str) -> Result<f32, String> {
    parse_ranged(value, MASTER_GAIN_RANGE_DB, "dB").map(f32::round)
}

fn parse_filter_q(value: &str) -> Result<f32, String> {
    parse_ranged(value, FILTER_Q_RANGE, "").map(round_to_half)
}

fn parse_volume_leveling(value: &str) -> Result<f32, String> {
    parse_ranged(value, VOLUME_LEVELING_RANGE, "dB").map(round_to_half)
}

/// `std::round(value * 2) / 2` — the rounding `setFilterQ` (`FxController.cpp:1827`) and
/// `setVolumeLeveling` (`:1791`) apply, matching their sliders' 0.5 step.
fn round_to_half(value: f32) -> f32 {
    (value * 2.0).round() / 2.0
}

fn parse_ranged(value: &str, range: (f32, f32), unit: &str) -> Result<f32, String> {
    let (min, max) = range;
    let parsed: f32 = value
        .trim()
        .parse()
        .map_err(|_| format!("expected a number, got `{value}`"))?;
    if !parsed.is_finite() || parsed < min || parsed > max {
        let unit = if unit.is_empty() {
            String::new()
        } else {
            format!(" {unit}")
        };
        return Err(format!("expected {min}..={max}{unit}, got `{value}`"));
    }
    Ok(parsed)
}

/// Split a `a:b,c:d` list into its pairs, rejecting anything that is not exactly two
/// colon-separated halves. The Windows parser splits on `,` then `:` and simply produces garbage
/// for malformed input (`FxController.cpp:536-553`); we refuse it instead, per §4.7.
fn split_pairs(list: &str) -> Result<Vec<(&str, &str)>, String> {
    if list.trim().is_empty() {
        return Err("expected at least one `key:value` pair".to_owned());
    }
    list.split(',')
        .map(|pair| {
            pair.split_once(':')
                .map(|(key, value)| (key.trim(), value.trim()))
                .ok_or_else(|| format!("expected `key:value`, got `{pair}`"))
        })
        .collect()
}

fn parse_band_index(value: &str) -> Result<usize, String> {
    let index: usize = value
        .parse()
        .map_err(|_| format!("expected a 0-based band index, got `{value}`"))?;
    if index < eq::MAX_BANDS {
        Ok(index)
    } else {
        Err(format!(
            "band index {index} is past the {} the equalizer has",
            eq::MAX_BANDS
        ))
    }
}

fn parse_band_frequencies(list: &str) -> Result<BandPairs, String> {
    split_pairs(list)?
        .into_iter()
        .map(|(band, hz)| {
            Ok((
                parse_band_index(band)?,
                parse_ranged(hz, BAND_FREQ_RANGE_HZ, "Hz")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(BandPairs)
}

fn parse_band_gains(list: &str) -> Result<BandPairs, String> {
    split_pairs(list)?
        .into_iter()
        .map(|(band, gain)| {
            Ok((
                parse_band_index(band)?,
                parse_ranged(gain, (eq::MIN_GAIN_DB, eq::MAX_GAIN_DB), "dB")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(BandPairs)
}

fn parse_effects(list: &str) -> Result<EffectPairs, String> {
    let pairs = split_pairs(list)?;
    if pairs.len() > MAX_EFFECT_PAIRS {
        return Err(format!(
            "expected at most {MAX_EFFECT_PAIRS} effects, got {}",
            pairs.len()
        ));
    }
    pairs
        .into_iter()
        .map(|(name, value)| {
            Ok((
                parse_effect_name(name)?,
                parse_ranged(value, EFFECT_RANGE, "")?,
            ))
        })
        .collect::<Result<Vec<_>, String>>()
        .map(EffectPairs)
}

/// The lower-cased effect names `applyConfig` accepts (`FxController.cpp:574-600`).
///
/// `clarity` is the GUI's name for the engine's `Fidelity` — the same split that makes
/// `status.json` report `effects.clarity` from `FxEffects::Fidelity` (`FxController.cpp:695`).
fn parse_effect_name(name: &str) -> Result<Effect, String> {
    match name.to_ascii_lowercase().as_str() {
        "fidelity" | "clarity" => Ok(Effect::Fidelity),
        "ambience" => Ok(Effect::Ambience),
        "surround" => Ok(Effect::Surround),
        "dynamicboost" | "dynamic_boost" => Ok(Effect::DynamicBoost),
        "bass" | "bassboost" | "bass_boost" => Ok(Effect::Bass),
        other => Err(format!(
            "unknown effect `{other}`; expected one of fidelity/clarity, ambience, surround, \
             dynamicboost/dynamic_boost or bass/bassboost/bass_boost"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolerance for every float assertion below. The values are slider steps (1.0, 0.5), so this
    /// is four orders of magnitude tighter than anything that could hide a rounding mistake.
    const EPS: f32 = 1e-6;

    fn parse(args: &[&str]) -> Cli {
        let mut argv = vec!["fxsound"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("command line should parse")
    }

    fn error(args: &[&str]) -> String {
        let mut argv = vec!["fxsound"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv)
            .expect_err("command line should be rejected")
            .to_string()
    }

    #[test]
    fn every_option_the_windows_build_documents_still_parses() {
        let cli = parse(&[
            "--power=1",
            "--preset=Bass Booster",
            "--output=Speakers (Realtek High Definition Audio)",
            "--view=2",
            "--language=fr",
            "--num_bands=15",
            "--balance=-6",
            "--filter_q=2.0",
            "--master_gain=3",
            "--volume_leveling=2.0",
            "--set_band_freq=0:60,1:150",
            "--set_band_gain=0:3.0,1:-2.5",
            "--set_effect=bass:7.5,ambience:4.0",
            "--run_minimized",
        ]);

        assert_eq!(cli.power, Some(PowerArg::On));
        assert_eq!(cli.preset.as_deref(), Some("Bass Booster"));
        assert_eq!(
            cli.output.as_deref(),
            Some("Speakers (Realtek High Definition Audio)")
        );
        assert_eq!(cli.view, Some(ViewMode::Pro));
        assert_eq!(cli.language.as_deref(), Some("fr"));
        assert_eq!(cli.num_bands, Some(15));
        assert!((cli.balance.unwrap() - -6.0).abs() < EPS);
        assert!((cli.filter_q.unwrap() - 2.0).abs() < EPS);
        assert!((cli.master_gain.unwrap() - 3.0).abs() < EPS);
        assert!((cli.volume_leveling.unwrap() - 2.0).abs() < EPS);
        assert_eq!(cli.set_band_freq.as_ref().unwrap().0.len(), 2);
        assert_eq!(cli.set_band_gain.as_ref().unwrap().0.len(), 2);
        assert_eq!(cli.set_effect.as_ref().unwrap().0.len(), 2);
        assert!(cli.run_minimized);
    }

    #[test]
    fn a_device_name_is_forwarded_verbatim_whatever_alphabet_it_is_in() {
        // Device descriptions come from ALSA/UCM in the system language; on the development
        // machine that is Cyrillic, and the same string names a sink and a source.
        let cyrillic = "fifine Microphone Аналоговый стерео";
        assert_eq!(
            parse(&[&format!("--output={cyrillic}")]).output.as_deref(),
            Some(cyrillic)
        );
        assert_eq!(
            parse(&["--output", cyrillic]).output.as_deref(),
            Some(cyrillic)
        );
        assert_eq!(
            parse(&["--output=alsa_input.usb-3142_fifine_Microphone-00.analog-stereo"]).commands(),
            vec![Command::Output(OutputCommand::Select(
                "alsa_input.usb-3142_fifine_Microphone-00.analog-stereo".to_owned()
            ))],
            "a node.name of either direction is just a name here; the controller resolves it"
        );
        // An empty name is no command at all, as `applyConfig` treats an empty --output.
        assert!(
            parse(&["--output="])
                .commands()
                .iter()
                .all(|c| !matches!(c, Command::Output(_)))
        );
    }

    #[test]
    fn the_preset_management_flags_and_status_parse_on_their_own() {
        assert!(parse(&["--overwrite_preset"]).overwrite_preset);
        assert!(parse(&["--undo_preset"]).undo_preset);
        assert!(parse(&["--delete_preset"]).delete_preset);
        assert!(parse(&["--status"]).status);
        assert_eq!(
            parse(&["--save_preset=My Preset"]).save_preset.as_deref(),
            Some("My Preset")
        );
        assert_eq!(
            parse(&["--rename_preset=New Name"])
                .rename_preset
                .as_deref(),
            Some("New Name")
        );
    }

    #[test]
    fn a_value_may_be_attached_with_an_equals_sign_or_separated_by_a_space() {
        // `docs/COMMAND_LINE_OPTIONS.md:7` only allows the first form; §4.7 says accept both.
        assert_eq!(parse(&["--preset=Rock"]).preset.as_deref(), Some("Rock"));
        assert_eq!(parse(&["--preset", "Rock"]).preset.as_deref(), Some("Rock"));
        assert_eq!(parse(&["--num_bands", "31"]).num_bands, Some(31));
    }

    #[test]
    fn the_hyphenated_spellings_are_accepted_as_hidden_aliases() {
        assert_eq!(parse(&["--num-bands=10"]).num_bands, Some(10));
        assert!(parse(&["--set-band-gain=0:1"]).set_band_gain.is_some());
        assert!(parse(&["--run-minimized"]).run_minimized);
        assert!(parse(&["--hide"]).run_minimized);
    }

    #[test]
    fn power_accepts_the_original_integer_space_and_the_linux_toggle() {
        assert_eq!(parse(&["--power=0"]).power, Some(PowerArg::Off));
        assert_eq!(parse(&["--power=1"]).power, Some(PowerArg::On));
        // `FxController.cpp:241-247` tests the int against zero, so anything non-zero is on.
        assert_eq!(parse(&["--power=7"]).power, Some(PowerArg::On));
        assert_eq!(parse(&["--power=toggle"]).power, Some(PowerArg::Toggle));
        assert_eq!(parse(&["--power=off"]).power, Some(PowerArg::Off));
        assert!(error(&["--power=yes"]).contains("expected 0, 1 or toggle"));
    }

    #[test]
    fn balance_and_master_gain_round_to_a_whole_number_of_decibels() {
        // `std::round` at `FxController.cpp:1803` and `:1815`.
        assert!((parse(&["--balance=6.4"]).balance.unwrap() - 6.0).abs() < EPS);
        assert!((parse(&["--balance=6.5"]).balance.unwrap() - 7.0).abs() < EPS);
        assert!((parse(&["--balance=-6.5"]).balance.unwrap() - -7.0).abs() < EPS);
        assert!((parse(&["--master_gain=-3.2"]).master_gain.unwrap() - -3.0).abs() < EPS);
    }

    #[test]
    fn filter_q_and_volume_leveling_round_to_the_nearest_half() {
        // `std::round(x * 2) / 2` at `FxController.cpp:1827` and `:1791`.
        assert!((parse(&["--filter_q=1.2"]).filter_q.unwrap() - 1.0).abs() < EPS);
        assert!((parse(&["--filter_q=1.3"]).filter_q.unwrap() - 1.5).abs() < EPS);
        assert!((parse(&["--filter_q=2.74"]).filter_q.unwrap() - 2.5).abs() < EPS);
        assert!((parse(&["--volume_leveling=0.24"]).volume_leveling.unwrap()).abs() < EPS);
        assert!((parse(&["--volume_leveling=3.9"]).volume_leveling.unwrap() - 4.0).abs() < EPS);
    }

    #[test]
    fn out_of_range_numbers_are_rejected_rather_than_silently_reset() {
        // Windows resets each of these to its default with no message
        // (`docs/COMMAND_LINE_OPTIONS.md:9`); §4.7 requires an error on the CLI path.
        assert!(error(&["--balance=25"]).contains("-20..=20 dB"));
        assert!(error(&["--balance=-20.5"]).contains("-20..=20 dB"));
        assert!(error(&["--master_gain=21"]).contains("-20..=20 dB"));
        assert!(error(&["--filter_q=0.9"]).contains("1..=3"));
        assert!(error(&["--filter_q=3.1"]).contains("1..=3"));
        assert!(error(&["--volume_leveling=4.6"]).contains("0..=4 dB"));
        assert!(error(&["--volume_leveling=-1"]).contains("0..=4 dB"));
        assert!(error(&["--balance=loud"]).contains("expected a number"));
    }

    #[test]
    fn the_band_count_must_be_one_of_the_five_the_equalizer_supports() {
        for bands in VALID_BAND_COUNTS {
            assert_eq!(
                parse(&[&format!("--num_bands={bands}")]).num_bands,
                Some(bands)
            );
        }
        assert!(error(&["--num_bands=7"]).contains("5, 10, 15, 20 or 31"));
        assert!(error(&["--num_bands=0"]).contains("5, 10, 15, 20 or 31"));
        assert!(error(&["--num_bands=ten"]).contains("whole number"));
    }

    #[test]
    fn the_view_option_maps_one_to_lite_and_two_to_pro() {
        assert_eq!(parse(&["--view=1"]).view, Some(ViewMode::Lite));
        assert_eq!(parse(&["--view=2"]).view, Some(ViewMode::Pro));
        assert!(error(&["--view=3"]).contains("1 (Lite) or 2 (Pro)"));
    }

    #[test]
    fn band_lists_parse_into_index_and_value_pairs() {
        let freq = parse(&["--set_band_freq=0:60,1:150.5"])
            .set_band_freq
            .unwrap();
        assert_eq!(freq.0[0].0, 0);
        assert!((freq.0[0].1 - 60.0).abs() < EPS);
        assert_eq!(freq.0[1].0, 1);
        assert!((freq.0[1].1 - 150.5).abs() < EPS);

        let gain = parse(&["--set_band_gain=0:3.0,9:-2.5"])
            .set_band_gain
            .unwrap();
        assert_eq!(gain.0[1].0, 9);
        assert!((gain.0[1].1 - -2.5).abs() < EPS);
    }

    #[test]
    fn band_lists_reject_malformed_input_and_out_of_range_values() {
        assert!(error(&["--set_band_freq=0"]).contains("expected `key:value`"));
        assert!(error(&["--set_band_freq=x:60"]).contains("0-based band index"));
        assert!(error(&["--set_band_freq=0:1"]).contains("20..=20000 Hz"));
        assert!(error(&["--set_band_gain=0:99"]).contains("-12..=12 dB"));
        assert!(error(&["--set_band_gain=32:0"]).contains("band index 32"));
        assert!(error(&["--set_band_gain="]).contains("at least one"));
    }

    #[test]
    fn every_documented_effect_alias_maps_to_its_effect() {
        let cases = [
            ("fidelity", Effect::Fidelity),
            ("clarity", Effect::Fidelity),
            ("ambience", Effect::Ambience),
            ("surround", Effect::Surround),
            ("dynamicboost", Effect::DynamicBoost),
            ("dynamic_boost", Effect::DynamicBoost),
            ("bass", Effect::Bass),
            ("bassboost", Effect::Bass),
            ("bass_boost", Effect::Bass),
        ];
        for (name, expected) in cases {
            let parsed = parse(&[&format!("--set_effect={name}:5")])
                .set_effect
                .unwrap();
            assert_eq!(
                parsed.0[0].0, expected,
                "`{name}` should map to {expected:?}"
            );
            assert!((parsed.0[0].1 - 5.0).abs() < EPS);
        }
    }

    #[test]
    fn the_effect_list_is_capped_at_five_pairs_and_validated() {
        let all_five = "fidelity:1,ambience:2,surround:3,dynamicboost:4,bass:5";
        assert_eq!(
            parse(&[&format!("--set_effect={all_five}")])
                .set_effect
                .unwrap()
                .0
                .len(),
            5
        );
        let six = format!("{all_five},clarity:6");
        assert!(error(&[&format!("--set_effect={six}")]).contains("at most 5 effects"));
        assert!(error(&["--set_effect=treble:5"]).contains("unknown effect `treble`"));
        assert!(error(&["--set_effect=bass:11"]).contains("0..=10"));
    }

    #[test]
    fn an_unknown_option_is_an_error() {
        assert!(error(&["--turbo"]).contains("--turbo"));
    }

    #[test]
    fn status_suppresses_every_other_command_on_the_line() {
        // `FxController.cpp:348-352` returns before anything else runs, which is also why the
        // window is not raised.
        let commands = parse(&["--status", "--power=1", "--preset=Rock"]).commands();
        assert_eq!(commands, vec![Command::Status { json: false }]);
    }

    #[test]
    fn commands_come_out_in_the_order_apply_config_applies_them() {
        let commands = parse(&[
            "--set_effect=bass:5",
            "--num_bands=31",
            "--set_band_gain=30:6",
            "--language=fr",
            "--power=1",
            "--view=1",
            "--preset=Rock",
        ])
        .commands();

        assert_eq!(
            commands,
            vec![
                Command::Power(PowerCommand::On),
                Command::Preset(PresetCommand::Select("Rock".to_owned())),
                Command::NumBands(31),
                Command::View(ViewMode::Lite),
                Command::Language("fr".to_owned()),
                Command::Window(WindowCommand::Show),
                Command::BandGains(vec![(30, 6.0)]),
                Command::Effects(vec![(Effect::Bass, 5.0)]),
            ],
            "the band count must land before the band list, per FxController.cpp:558"
        );
    }

    #[test]
    fn each_preset_option_on_its_own_makes_its_command() {
        for (args, expected) in [
            ("--preset=Rock", PresetCommand::Select("Rock".to_owned())),
            ("--save_preset=New", PresetCommand::SaveAs("New".to_owned())),
            ("--overwrite_preset", PresetCommand::Overwrite),
            ("--undo_preset", PresetCommand::Undo),
            (
                "--rename_preset=Other",
                PresetCommand::Rename("Other".to_owned()),
            ),
            ("--delete_preset", PresetCommand::Delete),
            ("--next-preset", PresetCommand::Next),
            ("--prev-preset", PresetCommand::Previous),
        ] {
            assert_eq!(
                parse(&[args]).commands(),
                vec![Command::Preset(expected)],
                "{args}"
            );
        }
    }

    #[test]
    fn two_preset_options_on_one_line_are_refused_with_both_named() {
        // 0.4.0 audit #30: `docs/COMMAND_LINE_OPTIONS.md:40` takes the first and drops the rest in
        // silence, so `--save_preset=A --preset=B` selected B and saved nothing.
        let all = [
            "--preset=Rock",
            "--save_preset=New",
            "--overwrite_preset",
            "--undo_preset",
            "--rename_preset=Other",
            "--delete_preset",
            "--next-preset",
            "--prev-preset",
        ];
        let option = |arg: &str| arg.split('=').next().unwrap_or(arg).to_owned();
        for (i, first) in all.iter().enumerate() {
            for second in &all[i + 1..] {
                let message = error(&[first, second]);
                assert!(
                    message.contains("cannot be used with"),
                    "{first} {second}: {message}"
                );
                assert!(
                    message.contains(&option(first)) && message.contains(&option(second)),
                    "{first} {second}: {message}"
                );
            }
        }
    }

    #[test]
    fn a_preset_name_is_stripped_and_truncated_before_it_is_forwarded() {
        assert_eq!(sanitise_preset_name("Mu:sic"), "Music");
        assert_eq!(sanitise_preset_name(r#"<>:"/\|?*Rock"#), "Rock");
        assert_eq!(sanitise_preset_name(&"a".repeat(80)).chars().count(), 64);

        let commands = parse(&["--save_preset=Mu:sic"]).commands();
        assert!(commands.contains(&Command::Preset(PresetCommand::SaveAs("Music".to_owned()))));
    }

    #[test]
    fn a_multi_line_preset_name_from_the_command_line_is_saved_on_one_line() {
        // `fxsound --save_preset="$(xclip -o)"` with two lines selected: the `.fac` it saved could
        // not be read back, and the preset vanished from the list.
        let commands = parse(&["--save_preset=Line one\nLine two\n"]).commands();
        assert_eq!(
            commands,
            vec![Command::Preset(PresetCommand::SaveAs(
                "Line one Line two".to_owned()
            ))]
        );
        let commands = parse(&["--rename_preset=Tab\there"]).commands();
        assert_eq!(
            commands,
            vec![Command::Preset(PresetCommand::Rename(
                "Tab here".to_owned()
            ))]
        );
    }

    #[test]
    fn a_new_preset_name_from_the_command_line_fits_what_windows_reads() {
        // 0.4.0 audit #15: sixty-four Cyrillic letters are 128 bytes, two more than the name line
        // a Windows FxSound reads.
        // Not called `long`: the man page check reads that word, an equals sign and a string as
        // an option's name.
        let typed = "я".repeat(64);
        let save = format!("--save_preset={typed}");
        let rename = format!("--rename_preset={typed}");
        let cut = "я".repeat(63);
        assert!(
            parse(&[save.as_str()])
                .commands()
                .contains(&Command::Preset(PresetCommand::SaveAs(cut.clone())))
        );
        assert!(
            parse(&[rename.as_str()])
                .commands()
                .contains(&Command::Preset(PresetCommand::Rename(cut.clone())))
        );
        assert!(cut.len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn a_name_that_is_empty_or_sanitises_away_is_refused_with_a_message() {
        // 0.4.0 audit #30: `--save_preset="::"` saved nothing and said nothing.
        for args in [
            "--save_preset=::",
            "--rename_preset=???",
            "--save_preset=  ",
            "--rename_preset=",
        ] {
            let message = error(&[args]);
            assert!(message.contains("is no preset name"), "{args}: {message}");
        }
        let message = error(&["--save_preset=\n\t"]);
        assert!(message.contains("`\\n\\t` is no preset name"), "{message}");
        assert!(error(&["--preset="]).contains("a preset name cannot be empty"));
        assert!(error(&["--preset", " "]).contains("a preset name cannot be empty"));
        // What sanitises to something is kept as typed and sanitised on its way into the command.
        let cli = parse(&["--save_preset= Mu:sic "]);
        assert_eq!(cli.save_preset.as_deref(), Some(" Mu:sic "));
        assert_eq!(
            cli.commands(),
            vec![Command::Preset(PresetCommand::SaveAs("Music".to_owned()))]
        );
    }

    #[test]
    fn only_show_view_and_a_line_with_no_options_raise_the_window() {
        // 0.4.0 audit R11: step 11 of applyConfig is an `else` (`FxController.cpp:523-531`), so on
        // Windows every option raised the window, a keybind's `--preset=Gaming` over a game too.
        for raising in [
            &[][..],
            &["--show"][..],
            &["--view=2"][..],
            &["--view=1", "--preset=Rock"][..],
            &["--power=1", "--show"][..],
            // An empty name makes no command, so this line asks for nothing but the window.
            &["--output="][..],
        ] {
            assert!(
                parse(raising)
                    .commands()
                    .contains(&Command::Window(WindowCommand::Show)),
                "{raising:?} should raise the window"
            );
        }
        assert!(
            parse(&["--run_minimized"])
                .commands()
                .contains(&Command::Window(WindowCommand::Hide))
        );
        assert!(
            parse(&["--toggle-window"])
                .commands()
                .contains(&Command::Window(WindowCommand::Toggle))
        );
    }

    #[test]
    fn every_option_that_sets_something_leaves_the_window_where_it_is() {
        // 0.4.0 audit R11: presets, power, devices, effects, bands, levels and the language are
        // state, and a script or a keybind sets them without wanting the window.
        for quiet in [
            &["--power=1"][..],
            &["--power=toggle"][..],
            &["--preset=Gaming"][..],
            &["--save_preset=Mine"][..],
            &["--overwrite_preset"][..],
            &["--undo_preset"][..],
            &["--rename_preset=Other"][..],
            &["--delete_preset"][..],
            &["--output=Speakers"][..],
            &["--input=Mic"][..],
            &["--output=off"][..],
            &["--forget-device=Old Dock"][..],
            &["--edit=input"][..],
            &["--language=fr"][..],
            &["--num_bands=31"][..],
            &["--balance=3"][..],
            &["--filter_q=2"][..],
            &["--master_gain=-6"][..],
            &["--volume_leveling=2"][..],
            &["--set_band_freq=0:60"][..],
            &["--set_band_gain=0:3"][..],
            &["--set_effect=bass:7"][..],
            &["--noise-suppression=strong"][..],
            &["--app-preset=bf6.exe=Gaming"][..],
            &[
                "--edit=output",
                "--preset=Rock",
                "--set_effect=bass:7,ambience:3",
            ][..],
        ] {
            let commands = parse(quiet).commands();
            assert!(!commands.is_empty(), "{quiet:?} does something");
            assert!(
                !commands.iter().any(|c| matches!(c, Command::Window(_))),
                "{quiet:?} must not touch the window: {commands:?}"
            );
        }
    }

    #[test]
    fn a_line_that_only_sets_something_starts_fxsound_in_the_tray_and_the_rest_do_not() {
        // FA: R11 covered a running instance; a cold start from `fxsound --preset=Gaming` still
        // opened and focused the window over the game.
        for quiet in [
            &["--preset=Gaming"][..],
            &["--toggle-power"][..],
            &["--power=toggle"][..],
            &["--next-preset"][..],
            &["--output=Speakers"][..],
            &["--forget-device=Old Dock", "--preset=Night"][..],
            &["--edit=input", "--noise-suppression=strong"][..],
        ] {
            assert!(parse(quiet).only_sets_things(), "{quiet:?}");
        }
        for about_the_window in [
            &[][..],
            &["--show"][..],
            &["--view=2"][..],
            &["--preset=Gaming", "--show"][..],
            &["--hide"][..],
            &["--run_minimized"][..],
            &["--toggle-window"][..],
            &["--activated"][..],
            &["--status"][..],
            &["--output="][..],
        ] {
            assert!(
                !parse(about_the_window).only_sets_things(),
                "{about_the_window:?}"
            );
        }
    }

    #[test]
    fn an_activated_start_is_a_hidden_start() {
        // What the D-Bus activation file and the user unit run: hidden, as `--hide`, and the
        // hiding is honoured at a cold start. As a second process it forwards nothing at all,
        // which `main` decides before anything is forwarded.
        let cli = parse(&["--activated"]);
        assert!(cli.activated);
        assert!(!cli.run_minimized);
        assert_eq!(
            cli.cold_start_commands(),
            [Command::Window(WindowCommand::Hide)]
        );
    }

    #[test]
    fn a_compositor_hotkey_alone_leaves_the_window_where_it_is() {
        for hotkey in [
            "--toggle-power",
            "--next-preset",
            "--prev-preset",
            "--next-output",
            "--quit",
        ] {
            let commands = parse(&[hotkey]).commands();
            assert!(
                !commands.iter().any(|c| matches!(c, Command::Window(_))),
                "`{hotkey}` stands in for a global hotkey and must not raise the window: {commands:?}"
            );
        }
        // Mixed with an option about the window, the window comes up.
        assert!(
            parse(&["--next-preset", "--view=2"])
                .commands()
                .contains(&Command::Window(WindowCommand::Show))
        );
    }

    #[test]
    fn the_compositor_options_produce_the_commands_the_hotkeys_produced() {
        assert_eq!(
            parse(&["--toggle-power"]).commands(),
            vec![Command::Power(PowerCommand::Toggle)]
        );
        assert_eq!(
            parse(&["--next-preset"]).commands(),
            vec![Command::Preset(PresetCommand::Next)]
        );
        assert_eq!(
            parse(&["--prev-preset"]).commands(),
            vec![Command::Preset(PresetCommand::Previous)]
        );
        assert_eq!(
            parse(&["--next-output"]).commands(),
            vec![Command::Output(OutputCommand::Next)]
        );
        assert_eq!(parse(&["--quit"]).commands(), vec![Command::Quit]);
    }

    #[test]
    fn an_explicit_power_value_wins_over_the_toggle() {
        assert_eq!(
            parse(&["--power=0", "--toggle-power"]).power_command(),
            Some(PowerCommand::Off)
        );
    }

    #[test]
    fn cold_start_carries_out_the_band_lists_the_effects_and_every_preset_command() {
        // None of these exist in `initConfig` (`FxController.cpp:225-234`), which drops them at a
        // start without a word; the manual page says every option works as a startup option.
        let cli = parse(&[
            "--overwrite_preset",
            "--set_band_gain=0:3",
            "--set_band_freq=0:60",
            "--set_effect=bass:5",
        ]);
        assert_eq!(
            cli.cold_start_commands(),
            vec![
                Command::Preset(PresetCommand::Overwrite),
                Command::BandFrequencies(vec![(0, 60.0)]),
                Command::BandGains(vec![(0, 3.0)]),
                Command::Effects(vec![(Effect::Bass, 5.0)]),
            ]
        );
        for preset in [
            &["--save_preset=Mine"][..],
            &["--undo_preset"][..],
            &["--rename_preset=Mine"][..],
            &["--delete_preset"][..],
            &["--next-preset"][..],
            &["--prev-preset"][..],
        ] {
            let cold = parse(preset).cold_start_commands();
            assert!(
                matches!(cold.as_slice(), [Command::Preset(_)]),
                "{preset:?}: {cold:?}"
            );
        }
        // A toggle of the window a start has not put up yet: `main` reads it as `--show`.
        assert_eq!(
            parse(&["--toggle-window"]).cold_start_commands(),
            vec![Command::Window(WindowCommand::Toggle)]
        );
    }

    #[test]
    fn cold_start_honours_the_options_init_config_reads() {
        let cli = parse(&[
            "--power=1",
            "--preset=Rock",
            "--output=Speakers",
            "--view=2",
            "--language=fr",
            "--num_bands=15",
            "--balance=2",
            "--filter_q=1.5",
            "--master_gain=1",
            "--volume_leveling=1",
            "--run_minimized",
        ]);
        assert_eq!(
            cli.cold_start_commands().len(),
            11,
            "every option in §4.3 survives a cold start"
        );
        assert!(
            cli.cold_start_commands()
                .contains(&Command::Window(WindowCommand::Hide))
        );

        // The implicit "raise the window" of a forwarded line must not reach a cold start, or the
        // persisted `run_minimized` would never be able to hide it (§7.1).
        assert!(
            !parse(&["--power=1"])
                .cold_start_commands()
                .iter()
                .any(|c| matches!(c, Command::Window(_)))
        );

        assert!(parse(&["--status"]).cold_start_commands().is_empty());
        // `--quit` with no instance to quit is decided in `main` before the engine starts; it
        // must not slip through here and start one.
        assert!(parse(&["--quit"]).cold_start_commands().is_empty());
    }

    #[test]
    fn input_names_a_microphone_and_off_detaches_either_lane() {
        assert_eq!(
            parse(&["--input=alsa_input.usb-fifine"]).commands(),
            vec![Command::Input(DeviceCommand::Select(
                "alsa_input.usb-fifine".to_owned()
            ))]
        );
        for off in ["off", "OFF", "Off"] {
            assert!(
                parse(&[&format!("--input={off}")])
                    .commands()
                    .contains(&Command::Input(DeviceCommand::Detach)),
                "--input={off}"
            );
            assert!(
                parse(&["--output", off])
                    .commands()
                    .contains(&Command::Output(DeviceCommand::Detach)),
                "--output {off}"
            );
        }
        // An empty name is no command, for `--input` as for `--output`.
        assert!(
            parse(&["--input="])
                .commands()
                .iter()
                .all(|c| !matches!(c, Command::Input(_)))
        );
    }

    #[test]
    fn next_input_cycles_the_input_lane_and_a_name_wins_over_it() {
        assert_eq!(
            parse(&["--next-input"]).commands(),
            vec![Command::Input(DeviceCommand::Next)]
        );
        assert_eq!(
            parse(&["--next_input"]).commands(),
            vec![Command::Input(DeviceCommand::Next)]
        );
        let both = parse(&["--next-input", "--input=Mic"]).commands();
        assert!(both.contains(&Command::Input(DeviceCommand::Select("Mic".to_owned()))));
        assert!(!both.contains(&Command::Input(DeviceCommand::Next)));
        // The two lanes' keybinds are independent.
        assert_eq!(
            parse(&["--next-output", "--next-input"]).commands(),
            vec![
                Command::Output(DeviceCommand::Next),
                Command::Input(DeviceCommand::Next),
            ]
        );
    }

    #[test]
    fn edit_takes_output_or_input_in_any_case() {
        assert_eq!(parse(&["--edit=input"]).edit, Some(DeviceDirection::Input));
        assert_eq!(
            parse(&["--edit", "Output"]).edit,
            Some(DeviceDirection::Output)
        );
        assert!(error(&["--edit=mic"]).contains("expected output or input"));
        assert!(
            parse(&["--edit=input"])
                .commands()
                .contains(&Command::EditDirection(DeviceDirection::Input))
        );
    }

    #[test]
    fn noise_suppression_takes_the_five_settings_keys_and_the_window_s_word_for_light() {
        for (value, expected) in [
            ("preset", NoiseSuppressionOverride::Preset),
            ("off", NoiseSuppressionOverride::Off),
            ("light", NoiseSuppressionOverride::Light),
            ("mild", NoiseSuppressionOverride::Light),
            ("medium", NoiseSuppressionOverride::Medium),
            ("Strong", NoiseSuppressionOverride::Strong),
        ] {
            assert_eq!(
                parse(&[&format!("--noise-suppression={value}")]).noise_suppression,
                Some(expected),
                "{value}"
            );
        }
        assert_eq!(
            parse(&["--noise_suppression", "off"]).noise_suppression,
            Some(NoiseSuppressionOverride::Off)
        );
        assert!(
            error(&["--noise-suppression=max"])
                .contains("expected preset, off, light, medium or strong")
        );
        assert_eq!(
            parse(&["--noise-suppression=strong"]).commands(),
            vec![Command::NoiseSuppression(NoiseSuppressionOverride::Strong)]
        );
    }

    #[test]
    fn json_goes_with_status_watch_or_self_test_and_is_an_error_alone() {
        // CI runs `fxsound --self-test --json` on every package; 0.3.0 tied `--json` to
        // `--status` and would have refused it.
        assert_eq!(
            parse(&["--self-test", "--json"]).commands(),
            vec![Command::SelfTest { json: true }]
        );
        assert_eq!(
            parse(&["--status", "--json"]).commands(),
            vec![Command::Status { json: true }]
        );
        assert_eq!(
            parse(&["--watch", "--json"]).commands(),
            vec![Command::Watch {
                json: true,
                meters: false
            }]
        );
        let alone = error(&["--json"]);
        assert!(
            alone.contains("--status") || alone.contains("--self-test"),
            "{alone}"
        );
        assert!(!error(&["--json", "--power=1"]).is_empty());
    }

    #[test]
    fn meters_goes_with_watch_only() {
        assert_eq!(
            parse(&["--watch", "--meters"]).commands(),
            vec![Command::Watch {
                json: false,
                meters: true
            }]
        );
        assert!(error(&["--meters"]).contains("--watch"));
        assert!(error(&["--status", "--meters"]).contains("--watch"));
    }

    #[test]
    fn self_test_status_and_watch_each_answer_alone_and_never_raise_the_window() {
        for (args, expected) in [
            (
                &["--self-test", "--power=1", "--show"][..],
                Command::SelfTest { json: false },
            ),
            (
                &["--status", "--input=Mic", "--next-input"][..],
                Command::Status { json: false },
            ),
            (
                &["--watch", "--preset=Rock", "--edit=input"][..],
                Command::Watch {
                    json: false,
                    meters: false,
                },
            ),
            // One question per line: the self-test first, it never contacts an instance.
            (
                &["--status", "--watch", "--self-test"][..],
                Command::SelfTest { json: false },
            ),
            (
                &["--watch", "--status"][..],
                Command::Status { json: false },
            ),
        ] {
            let cli = parse(args);
            assert!(cli.is_query(), "{args:?}");
            assert_eq!(cli.commands(), vec![expected], "{args:?}");
            assert!(cli.window_command().is_none(), "{args:?}");
        }
        assert!(!parse(&["--power=1"]).is_query());
    }

    #[test]
    fn self_test_has_a_hidden_underscore_spelling() {
        assert!(parse(&["--self_test"]).self_test);
    }

    #[test]
    fn devices_come_before_the_preset_and_edit_sits_between() {
        // A device says which lane the line means, so the preset is looked up after it; an
        // explicit `--edit` wins over the lane a device picked.
        let commands = parse(&[
            "--preset=Noisy Room",
            "--noise-suppression=strong",
            "--edit=input",
            "--input=Mic",
            "--output=Speakers",
            "--power=1",
            "--num_bands=15",
        ])
        .commands();
        assert_eq!(
            commands,
            vec![
                Command::Power(PowerCommand::On),
                Command::Output(DeviceCommand::Select("Speakers".to_owned())),
                Command::Input(DeviceCommand::Select("Mic".to_owned())),
                Command::EditDirection(DeviceDirection::Input),
                Command::Preset(PresetCommand::Select("Noisy Room".to_owned())),
                Command::NoiseSuppression(NoiseSuppressionOverride::Strong),
                Command::NumBands(15),
            ]
        );
    }

    #[test]
    fn the_new_hotkey_shaped_options_leave_the_window_alone_and_so_do_the_device_ones() {
        for quiet in [
            &["--next-input"][..],
            &["--noise-suppression=off"][..],
            &[
                "--next-input",
                "--next-output",
                "--noise-suppression=strong",
            ][..],
            &["--input=Mic"][..],
            &["--edit=input"][..],
            &["--output=off"][..],
            &["--next-input", "--input=Mic"][..],
            &["--noise-suppression=off", "--edit=input"][..],
        ] {
            let commands = parse(quiet).commands();
            assert!(
                !commands.iter().any(|c| matches!(c, Command::Window(_))),
                "{quiet:?} must not raise the window: {commands:?}"
            );
        }
    }

    #[test]
    fn a_cold_start_keeps_an_explicit_show_and_drops_the_raise_of_a_bare_line() {
        // `--show` overrides a remembered "start hidden"; a bare `fxsound`, which a desktop entry
        // runs, and `--view` leave the choice to the setting (§7.1).
        assert_eq!(
            parse(&["--show"]).cold_start_commands(),
            vec![Command::Window(WindowCommand::Show)]
        );
        assert!(parse(&[]).cold_start_commands().is_empty());
        assert_eq!(
            parse(&["--view=1"]).cold_start_commands(),
            vec![Command::View(ViewMode::Lite)]
        );
        assert_eq!(
            parse(&["--hide", "--show"]).cold_start_commands(),
            vec![Command::Window(WindowCommand::Hide)],
            "hiding wins, as it does for a running instance"
        );
    }

    #[test]
    fn language_takes_a_code_an_iso_code_a_locale_or_system() {
        // 0.4.0 audit #28: `--language=uk` was saved, found no table, and showed the system's
        // language without a word.
        for (value, code) in [
            ("fr", "fr"),
            ("RU", "ru"),
            ("pt-BR", "pt-br"),
            ("zh-tw", "zh-TW"),
            ("uk", "ua"),
            ("bs", "ba"),
            ("nb", "no"),
            ("nn", "no"),
            ("de_AT.UTF-8", "de"),
            ("system", "system"),
            ("Default", "system"),
        ] {
            assert_eq!(
                parse(&[&format!("--language={value}")]).language.as_deref(),
                Some(code),
                "{value}"
            );
        }
        for unknown in ["hu", "xx", "klingon", ""] {
            let message = error(&[&format!("--language={unknown}")]);
            assert!(
                message.contains("no translation for") && message.contains("zh-TW"),
                "{unknown}: {message}"
            );
        }
        assert_eq!(
            parse(&["--language=uk"]).commands(),
            vec![Command::Language("ua".to_owned())]
        );
    }

    #[test]
    fn forget_device_names_a_device_and_is_left_to_main_at_a_cold_start() {
        assert_eq!(
            parse(&["--forget-device", "Old Dock"]).commands(),
            vec![Command::ForgetDevice("Old Dock".to_owned())]
        );
        // Done to the settings file before the engine starts, not in step 4, where no device
        // has been listed and it could only be refused.
        assert_eq!(
            parse(&["--forget_device=alsa_output.usb-dock"]).commands(),
            vec![Command::ForgetDevice("alsa_output.usb-dock".to_owned())]
        );
        assert!(
            parse(&["--forget_device=alsa_output.usb-dock"])
                .cold_start_commands()
                .is_empty()
        );
        assert_eq!(
            parse(&["--forget-device=Old Dock", "--power=on"]).cold_start_commands(),
            vec![Command::Power(PowerCommand::On)]
        );
        assert!(error(&["--forget-device="]).contains("a device name cannot be empty"));
    }

    #[test]
    fn cold_start_honours_the_lane_options_but_leaves_cycling_and_questions_to_main() {
        let cli = parse(&[
            "--input=Mic",
            "--output=off",
            "--edit=input",
            "--noise-suppression=light",
        ]);
        assert_eq!(
            cli.cold_start_commands(),
            vec![
                Command::Output(DeviceCommand::Detach),
                Command::Input(DeviceCommand::Select("Mic".to_owned())),
                Command::EditDirection(DeviceDirection::Input),
                Command::NoiseSuppression(NoiseSuppressionOverride::Light),
            ]
        );
        assert!(
            parse(&["--input=off"]).cold_start_commands()
                == vec![Command::Input(DeviceCommand::Detach)]
        );
        for args in [
            &["--next-input"][..],
            &["--next-output"][..],
            &["--watch"][..],
            &["--watch", "--meters", "--json"][..],
            &["--self-test"][..],
        ] {
            assert!(
                parse(args).cold_start_commands().is_empty(),
                "{args:?} is not a cold-start option"
            );
        }
    }

    // ---- per-application presets ---------------------------------------------------------------

    fn app_preset(direction: DeviceDirection, app: &str, preset: Option<&str>) -> Command {
        Command::AppPreset {
            direction,
            app: app.to_owned(),
            preset: preset.map_or(AppPresetChoice::Follow, |name| {
                AppPresetChoice::Preset(name.to_owned())
            }),
        }
    }

    #[test]
    fn app_preset_takes_app_equals_preset_and_each_option_may_be_given_more_than_once() {
        let commands = parse(&[
            "--app-input-preset=com.discordapp.Discord=Headset",
            "--app-preset",
            "bf6.exe=Gaming",
            "--app-preset=Brave=Volume Boost",
        ])
        .commands();
        assert_eq!(
            commands,
            vec![
                app_preset(DeviceDirection::Output, "bf6.exe", Some("Gaming")),
                app_preset(DeviceDirection::Output, "Brave", Some("Volume Boost")),
                app_preset(
                    DeviceDirection::Input,
                    "com.discordapp.Discord",
                    Some("Headset")
                ),
            ],
            "the output lane's in the order given, then the input lane's, and no window"
        );
    }

    #[test]
    fn default_follow_and_nothing_after_the_equals_sign_follow_the_lane() {
        for value in [
            "bf6.exe=default",
            "bf6.exe=Default",
            "bf6.exe=FOLLOW",
            "bf6.exe=",
            "bf6.exe=  default ",
        ] {
            assert_eq!(
                parse(&["--app-preset", value]).app_preset,
                vec![AppRuleArg {
                    app: "bf6.exe".to_owned(),
                    preset: AppPresetChoice::Follow,
                }],
                "{value}"
            );
        }
        assert_eq!(
            AppPresetChoice::parse("Defaults"),
            AppPresetChoice::Preset("Defaults".to_owned()),
            "only the whole word"
        );
        assert_eq!(AppPresetChoice::Follow.name(), None);
        assert_eq!(
            AppPresetChoice::Preset("Gaming".to_owned()).name(),
            Some("Gaming")
        );
    }

    #[test]
    fn the_application_is_everything_before_the_first_equals_sign_trimmed() {
        assert_eq!(
            parse(&["--app-input-preset= Battlefield 6 = Bass=Max "]).app_input_preset,
            vec![AppRuleArg {
                app: "Battlefield 6".to_owned(),
                preset: AppPresetChoice::Preset("Bass=Max".to_owned()),
            }]
        );
    }

    #[test]
    fn an_app_rule_without_an_equals_sign_or_an_application_is_an_error() {
        assert!(error(&["--app-preset=Gaming"]).contains("expected APP=PRESET"));
        assert!(error(&["--app-preset", "=Gaming"]).contains("an application before the `=`"));
        assert!(error(&["--app-input-preset", "  =Headset"]).contains("APP=PRESET"));
        assert!(error(&["--app-preset"]).contains("APP=PRESET"));
    }

    #[test]
    fn the_app_options_have_hidden_underscore_spellings() {
        let cli = parse(&[
            "--app_preset=brave=Music",
            "--app_input_preset=discord=Headset",
            "--list_apps",
        ]);
        assert_eq!(cli.app_preset.len(), 1);
        assert_eq!(cli.app_input_preset.len(), 1);
        assert!(cli.list_apps);
    }

    #[test]
    fn an_app_preset_leaves_the_window_alone_unless_the_line_says_otherwise() {
        for quiet in [
            &["--app-preset=bf6.exe=Gaming"][..],
            &["--app-input-preset=discord=default"][..],
            &[
                "--app-preset=bf6.exe=Gaming",
                "--next-preset",
                "--toggle-power",
            ][..],
        ] {
            let cli = parse(quiet);
            assert!(!cli.is_query(), "{quiet:?} is something to do");
            assert!(
                !cli.commands()
                    .iter()
                    .any(|command| matches!(command, Command::Window(_))),
                "{quiet:?} must not raise the window"
            );
        }
        for (line, window) in [
            (
                &["--app-preset=bf6.exe=Gaming", "--view=2"][..],
                WindowCommand::Show,
            ),
            (
                &["--app-preset=bf6.exe=Gaming", "--show"][..],
                WindowCommand::Show,
            ),
            (
                &["--app-preset=bf6.exe=Gaming", "--hide"][..],
                WindowCommand::Hide,
            ),
        ] {
            assert!(
                parse(line).commands().contains(&Command::Window(window)),
                "{line:?}"
            );
        }
    }

    #[test]
    fn an_app_preset_comes_after_the_preset_so_a_preset_saved_on_the_line_can_be_given() {
        let commands = parse(&[
            "--app-preset=bf6.exe=Night",
            "--save_preset=Night",
            "--edit=output",
        ])
        .commands();
        assert_eq!(
            commands,
            vec![
                Command::EditDirection(DeviceDirection::Output),
                Command::Preset(PresetCommand::SaveAs("Night".to_owned())),
                app_preset(DeviceDirection::Output, "bf6.exe", Some("Night")),
            ]
        );
    }

    #[test]
    fn list_apps_answers_alone_takes_json_and_never_raises_the_window() {
        for (args, expected) in [
            (&["--list-apps"][..], Command::ListApps { json: false }),
            (
                &[
                    "--list-apps",
                    "--json",
                    "--app-preset=bf6.exe=Gaming",
                    "--show",
                ][..],
                Command::ListApps { json: true },
            ),
            // A question answered once wins over the stream; `--status` over this one.
            (
                &["--watch", "--list-apps"][..],
                Command::ListApps { json: false },
            ),
            (
                &["--list-apps", "--status", "--json"][..],
                Command::Status { json: true },
            ),
        ] {
            let cli = parse(args);
            assert!(cli.is_query(), "{args:?}");
            assert_eq!(cli.commands(), vec![expected], "{args:?}");
            assert!(cli.window_command().is_none(), "{args:?}");
        }
    }

    #[test]
    fn cold_start_honours_an_app_preset_and_leaves_list_apps_to_main() {
        assert_eq!(
            parse(&[
                "--app-preset=bf6.exe=Gaming",
                "--app-input-preset=discord=default"
            ])
            .cold_start_commands(),
            vec![
                app_preset(DeviceDirection::Output, "bf6.exe", Some("Gaming")),
                app_preset(DeviceDirection::Input, "discord", None),
            ]
        );
        assert!(
            parse(&["--list-apps", "--json"])
                .cold_start_commands()
                .is_empty()
        );
    }

    // ---- «Как в Windows» (A9) ---------------------------------------------------------------

    #[test]
    fn windows_parity_reads_its_four_levels_in_any_case_and_everything_for_full() {
        // Read, all four: `full` is refused by the application, with the text every path
        // shares, not by the parser as an unknown word.
        for (value, level) in [
            ("off", WindowsParity::Off),
            ("Interface", WindowsParity::Interface),
            ("SOUND", WindowsParity::Sound),
            ("full", WindowsParity::Full),
            ("everything", WindowsParity::Full),
        ] {
            assert_eq!(
                parse(&[&format!("--windows-parity={value}")]).windows_parity,
                Some(level)
            );
            // A space works as well as an `=`, and so does the underscored spelling.
            assert_eq!(
                parse(&["--windows_parity", value]).windows_parity,
                Some(level)
            );
        }
        assert_eq!(
            parse(&["--windows-parity=full", "--force"]).commands(),
            [Command::WindowsParity {
                level: WindowsParity::Full,
                force: true
            }]
        );
    }

    #[test]
    fn an_unknown_level_is_refused_naming_the_three_offered() {
        let message = error(&["--windows-parity=windows"]);
        assert!(message.contains("off, interface or sound"), "{message}");
    }

    #[test]
    fn force_without_windows_parity_is_refused() {
        let message = error(&["--force"]);
        assert!(message.contains("--windows-parity"), "{message}");
    }

    #[test]
    fn windows_parity_sets_the_level_silently_as_every_set_option_does() {
        let cli = parse(&["--windows-parity=sound"]);
        assert!(cli.only_sets_things());
        assert_eq!(cli.window_command(), None);
        assert!(
            Command::WindowsParity {
                level: WindowsParity::Sound,
                force: false
            }
            .honoured_at_cold_start()
        );
    }

    #[test]
    fn every_command_declares_the_level_that_changes_it() {
        // The classification is an exhaustive match, so a new command cannot compile without a
        // level; this pins the decisions of the contract (`docs/0.5.0-windows-parity.md`).
        use ParityClass::{Full, Interface, Never};
        let cases: Vec<(Command, ParityClass)> = vec![
            (Command::Status { json: true }, Never),
            (
                Command::Watch {
                    json: true,
                    meters: false,
                },
                Never,
            ),
            (Command::SelfTest { json: false }, Never),
            (Command::ListApps { json: true }, Full),
            (Command::Power(PowerCommand::On), Interface),
            (Command::Power(PowerCommand::Toggle), Never),
            (
                Command::Preset(PresetCommand::Select("Rock".into())),
                Interface,
            ),
            (
                Command::Preset(PresetCommand::SaveAs("Mine".into())),
                Interface,
            ),
            (Command::Preset(PresetCommand::Next), Never),
            (
                Command::Output(DeviceCommand::Select("Speakers".into())),
                Interface,
            ),
            (Command::Output(DeviceCommand::Next), Never),
            (Command::Output(DeviceCommand::Detach), Never),
            (Command::Input(DeviceCommand::Select("Mic".into())), Full),
            (Command::Input(DeviceCommand::Next), Full),
            (Command::EditDirection(DeviceDirection::Input), Full),
            (
                Command::NoiseSuppression(NoiseSuppressionOverride::Strong),
                Full,
            ),
            (
                Command::AppPreset {
                    direction: DeviceDirection::Output,
                    app: "firefox".into(),
                    preset: AppPresetChoice::Follow,
                },
                Full,
            ),
            (Command::ForgetDevice("Old".into()), Never),
            (Command::NumBands(31), Interface),
            (Command::Language("fr".into()), Interface),
            (Command::View(ViewMode::Lite), Interface),
            (Command::BandGains(vec![(0, 3.0)]), Interface),
            (Command::Effects(vec![(Effect::Bass, 5.0)]), Interface),
            (
                Command::WindowsParity {
                    level: WindowsParity::Full,
                    force: false,
                },
                Never,
            ),
            (Command::Window(WindowCommand::Show), Never),
            (Command::Quit, Never),
        ];
        for (command, class) in cases {
            assert_eq!(command.parity_class(), class, "{command:?}");
        }
        // Nothing on the command line is a sound thing: the sound levels change what a preset
        // sounds like, not what an option does.
        assert!(
            parse(&["--preset=Rock", "--set_effect=bass:5", "--balance=2"])
                .commands()
                .iter()
                .all(|command| command.parity_class() != ParityClass::Sound)
        );
    }
}
