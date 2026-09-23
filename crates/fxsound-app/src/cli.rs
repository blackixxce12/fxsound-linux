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
//! `docs/spec/07-startup-tray.md` §4.7:
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
//!    `--quit`. `packaging/hyprland.conf.example` binds four of them already.
//!
//! Rounding is *not* a departure: `--balance` and `--master_gain` round to the nearest whole
//! number (`FxController.cpp:1803`, `:1815`) and `--filter_q` and `--volume_leveling` to the
//! nearest `0.5` (`FxController.cpp:1827`, `:1791`), matching the step size of the equivalent
//! sliders. The rounding happens in the value parser so a forwarded [`Command`] already carries
//! the value the DSP will see.

use clap::Parser;
use fxsound_core::{DeviceDirection, Effect, NoiseSuppressionOverride, ViewMode, eq};

/// The reserved-character set, the length cap and the sanitiser a preset name goes through
/// before it is used (`FxController::sanitizePresetName`, `fxsound/Source/GUI/FxController.cpp:378`).
///
/// They live in `fxsound_preset` beside the store that turns a name into a filename, so that a
/// name saved from the command line and one saved from the window become the same file. 0.3.0
/// kept a second sanitiser here that stripped nine characters while the store replaced three,
/// and a `--save_preset` name and a window-typed name could land in two files.
pub use fxsound_preset::{MAX_PRESET_NAME_LEN, PRESET_NAME_RESERVED, sanitise_preset_name};

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
/// `0.0..=1.0`; convert with [`fxsound_core::scale::slider_to_value`].
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
    // The three reports `--json` can reshape. A group rather than three `requires`, because clap
    // reads a list of `requires` as "all of them".
    group(clap::ArgGroup::new("report").args(["status", "watch", "self_test"]).multiple(true))
)]
pub struct Cli {
    /// Turn audio processing on or off. Any non-zero integer means on.
    ///
    /// `--power=<0|1>` — `docs/COMMAND_LINE_OPTIONS.md:17`, `FxController.cpp:241-247`, `:367-373`.
    #[arg(long = "power", value_name = "0|1|toggle", value_parser = parse_power)]
    pub power: Option<PowerArg>,

    /// Select a preset by its exact, case-sensitive name.
    ///
    /// `--preset=<name>` — `docs/COMMAND_LINE_OPTIONS.md:18`, `FxController.cpp:249-252`, `:395-401`.
    #[arg(long = "preset", value_name = "NAME")]
    pub preset: Option<String>,

    /// Save the current modified settings as a new user preset.
    ///
    /// `--save_preset=<name>` — `docs/COMMAND_LINE_OPTIONS.md:19`, `FxController.cpp:402-412`.
    #[arg(long = "save_preset", alias = "save-preset", value_name = "NAME")]
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
    #[arg(long = "rename_preset", alias = "rename-preset", value_name = "NAME")]
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

    /// Display language, e.g. `en`, `fr`, `fi`.
    ///
    /// `--language=<code>` — `docs/COMMAND_LINE_OPTIONS.md:26`, `FxController.cpp:269-278`.
    #[arg(long = "language", value_name = "CODE")]
    pub language: Option<String>,

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

    /// Print the `--status`, `--watch` or `--self-test` report as JSON rather than as lines.
    ///
    /// Deliberately not the default: the line format is what every existing script greps, and
    /// changing it under them would be a breaking change to buy a format they did not ask for.
    /// Given without any of the three it is an error, not a no-op.
    #[arg(long = "json", requires = "report")]
    pub json: bool,

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

    /// Show and raise the window of the running instance.
    ///
    /// Linux addition (`docs/spec/07-startup-tray.md` §4.7). The original has no such option
    /// because *every* forwarded command line except `--status` raises the window; see
    /// `Cli::window_command`.
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
    Power(PowerCommand),
    Preset(PresetCommand),
    Output(OutputCommand),
    Input(InputCommand),
    /// The lane the window edits and the preset and level commands act on.
    EditDirection(DeviceDirection),
    /// The microphone's global noise-suppression override.
    NoiseSuppression(NoiseSuppressionOverride),
    NumBands(u32),
    VolumeLeveling(f32),
    Balance(f32),
    FilterQ(f32),
    MasterGain(f32),
    View(ViewMode),
    Language(String),
    Window(WindowCommand),
    /// `(band index, Hz)` pairs. The caller must still drop the whole list when it is longer than
    /// the current band count (`FxController.cpp:536-553`) — that count is not knowable here.
    BandFrequencies(Vec<(usize, f32)>),
    /// `(band index, dB)` pairs, same size guard as [`Command::BandFrequencies`].
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

/// The six mutually exclusive preset commands, plus the two the compositor drives.
///
/// `docs/COMMAND_LINE_OPTIONS.md:40`: only one is processed per invocation, in the order the
/// `if`/`else if` chain at `FxController.cpp:395-448` tests them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetCommand {
    Select(String),
    /// Name already run through [`sanitise_preset_name`]; the case-insensitive collision check
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
    ///   raised (`:348-352`). `--self-test` and `--watch` are questions of the same kind and do
    ///   the same; with more than one on a line, `--self-test` wins, then `--status`.
    /// * the band and effect lists come *after* `--num_bands`, so
    ///   `--num_bands=31 --set_band_gain="30:6"` works in a single invocation (`:558`).
    ///
    /// One is deliberately not: the devices come *before* the preset, where `applyConfig` selects
    /// the preset first (`:395` before `:451`). With a lane per direction, a device is how a line
    /// says which lane it means — `--input=Headset --preset='Gaming Headset'` has to find the
    /// preset in the microphone's list — and a device brings back the preset remembered for it,
    /// which in the original order silently replaced the preset the same line had just asked for.
    /// `--edit` sits between the two, so that it wins over the lane a device picked.
    #[must_use]
    pub fn commands(&self) -> Vec<Command> {
        if self.self_test {
            return vec![Command::SelfTest { json: self.json }];
        }
        if self.status {
            return vec![Command::Status { json: self.json }];
        }
        if self.watch {
            return vec![Command::Watch {
                json: self.json,
                meters: self.meters,
            }];
        }

        let mut commands = Vec::new();
        if let Some(power) = self.power_command() {
            commands.push(Command::Power(power));
        }
        if let Some(output) = self.output_command() {
            commands.push(Command::Output(output));
        }
        if let Some(input) = self.input_command() {
            commands.push(Command::Input(input));
        }
        if let Some(direction) = self.edit {
            commands.push(Command::EditDirection(direction));
        }
        if let Some(preset) = self.preset_command() {
            commands.push(Command::Preset(preset));
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
        if let Some(window) = self.window_command() {
            commands.push(Command::Window(window));
        }
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
    /// command line gets for nothing (0.4.0 design §9, and [`Cli::window_command`]). A bus call
    /// comes from a keybind or a status bar, as every other method's does; `--show`,
    /// `--toggle-window` and `--hide` on the line still do what they say.
    #[must_use]
    pub fn commands_without_implicit_raise(&self) -> Vec<Command> {
        let mut commands = self.commands();
        if !self.show && self.window_command() == Some(WindowCommand::Show) {
            commands.retain(|command| *command != Command::Window(WindowCommand::Show));
        }
        commands
    }

    /// The subset of [`Cli::commands`] a *cold start* honours — the C column of
    /// `docs/spec/07-startup-tray.md` §4.2, i.e. what `initConfig` reads (`:225-234`).
    ///
    /// `initConfig` never sees `--status`, the preset-management commands, `--set_band_*` or
    /// `--set_effect`: at that point in startup there is no preset list, no audio and no window
    /// (`Main.cpp:67` runs it two lines before `AudioPassthru` exists).
    #[must_use]
    pub fn cold_start_commands(&self) -> Vec<Command> {
        self.commands()
            .into_iter()
            .filter(Command::honoured_at_cold_start)
            .collect()
    }

    /// `true` when this invocation only asks for state and must not disturb the window.
    #[must_use]
    pub const fn is_status(&self) -> bool {
        self.status
    }

    /// `true` when this invocation is a question — `--status`, `--watch` or `--self-test` —
    /// rather than something to do, so the window is left alone.
    #[must_use]
    pub const fn is_query(&self) -> bool {
        self.status || self.watch || self.self_test
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

    /// The `if`/`else if` chain of `FxController.cpp:395-448`: strictly the first match wins and
    /// every other preset option on the line is silently ignored
    /// (`docs/COMMAND_LINE_OPTIONS.md:40`).
    ///
    /// A name that sanitises away to nothing produces *no* command rather than falling through to
    /// the next option, because on Windows the chain has already matched by then (`:404`, `:431`).
    fn preset_command(&self) -> Option<PresetCommand> {
        if let Some(name) = &self.preset {
            (!name.is_empty()).then(|| PresetCommand::Select(name.clone()))
        } else if let Some(name) = &self.save_preset {
            let name = sanitise_preset_name(name);
            (!name.is_empty()).then_some(PresetCommand::SaveAs(name))
        } else if self.overwrite_preset {
            Some(PresetCommand::Overwrite)
        } else if self.undo_preset {
            Some(PresetCommand::Undo)
        } else if let Some(name) = &self.rename_preset {
            let name = sanitise_preset_name(name);
            (!name.is_empty()).then_some(PresetCommand::Rename(name))
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

    /// Step 11 of `applyConfig` (`FxController.cpp:523-531`) — and the one place this port
    /// knowingly narrows the original's behaviour.
    ///
    /// On Windows that step is an `else`: *any* forwarded command line that is not `--status`
    /// shows and raises the window, so `fxsound --power=1` pops the UI to the front. That is fine
    /// when `--power=1` can only come from a person typing it, but on Linux the same CLI is the
    /// replacement for the five global hotkeys (`docs/spec/07-startup-tray.md` §2.7), and a hotkey
    /// on Windows goes through `eventCallback` (`:1923-2014`) and never raises anything. So an
    /// invocation made *only* of compositor stand-ins leaves the window alone; add any of the
    /// original's options, or none at all, and the original's raise-the-window behaviour applies.
    fn window_command(&self) -> Option<WindowCommand> {
        if self.is_query() {
            None
        } else if self.run_minimized {
            Some(WindowCommand::Hide)
        } else if self.toggle_window {
            Some(WindowCommand::Toggle)
        } else if self.show {
            Some(WindowCommand::Show)
        } else if self.is_hotkey_substitute_only() {
            None
        } else {
            Some(WindowCommand::Show)
        }
    }

    /// `true` when the line carries at least one compositor stand-in and nothing that raises the
    /// window.
    ///
    /// The stand-ins are the five hotkeys' replacements plus the two 0.4.0 options shaped like
    /// them: `--next-input`, the input lane's `--next-output`, and `--noise-suppression`, a
    /// setting a keybinding flips without wanting the window. `--input` and `--edit` raise it, as
    /// `--output` always has: both are about what the window is showing.
    fn is_hotkey_substitute_only(&self) -> bool {
        let substitutes = self.toggle_power
            || self.next_preset
            || self.prev_preset
            || self.next_output
            || self.next_input
            || self.noise_suppression.is_some()
            || self.quit;
        substitutes && !self.has_original_option() && self.input.is_none() && self.edit.is_none()
    }

    /// Every option in `docs/COMMAND_LINE_OPTIONS.md`'s table, i.e. everything the Windows build
    /// could be handed.
    fn has_original_option(&self) -> bool {
        self.power.is_some()
            || self.preset.is_some()
            || self.save_preset.is_some()
            || self.overwrite_preset
            || self.undo_preset
            || self.rename_preset.is_some()
            || self.delete_preset
            || self.output.is_some()
            || self.view.is_some()
            || self.language.is_some()
            || self.num_bands.is_some()
            || self.balance.is_some()
            || self.filter_q.is_some()
            || self.master_gain.is_some()
            || self.volume_leveling.is_some()
            || self.set_band_freq.is_some()
            || self.set_band_gain.is_some()
            || self.set_effect.is_some()
            || self.status
            || self.run_minimized
    }
}

impl Command {
    /// Whether `initConfig` would act on this command, as opposed to `applyConfig` only.
    ///
    /// The list is `docs/spec/07-startup-tray.md` §4.3 verbatim. [`WindowCommand::Show`] is
    /// pointedly *not* on it: a plain `fxsound` emits one (see `Cli::window_command`), and
    /// honouring it at startup would override the persisted `run_minimized` on every launch and
    /// break "quit with the window hidden, start hidden next time" (§7.1). A cold start's
    /// visibility comes from the setting, with `--run_minimized` as the only override.
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
            Self::Preset(preset) => matches!(preset, PresetCommand::Select(_)),
            // `--output` is on §4.3's list, `--input` is its twin, and `off` is a choice of
            // device like any other. Cycling is a hotkey's, and a cold start has no hotkeys.
            Self::Output(device) | Self::Input(device) => {
                matches!(device, DeviceCommand::Select(_) | DeviceCommand::Detach)
            }
            // Settings a start-up line may well carry, like `--view` and `--balance`.
            Self::EditDirection(_) | Self::NoiseSuppression(_) => true,
            Self::Window(window) => matches!(window, WindowCommand::Hide),
            // Questions for a running instance, which a cold start proves there is not; `main`
            // answers all three before the engine starts.
            Self::Status { .. }
            | Self::Watch { .. }
            | Self::SelfTest { .. }
            | Self::BandFrequencies(_)
            | Self::BandGains(_)
            | Self::Effects(_)
            | Self::Quit => false,
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
            vec![
                Command::Output(OutputCommand::Select(
                    "alsa_input.usb-3142_fifine_Microphone-00.analog-stereo".to_owned()
                )),
                Command::Window(WindowCommand::Show),
            ],
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
    fn the_preset_commands_are_mutually_exclusive_in_the_documented_priority_order() {
        // `docs/COMMAND_LINE_OPTIONS.md:40` — first match wins, the rest are silently ignored.
        let all = [
            "--preset=Rock",
            "--save_preset=New",
            "--overwrite_preset",
            "--undo_preset",
            "--rename_preset=Other",
            "--delete_preset",
        ];
        let expected = [
            PresetCommand::Select("Rock".to_owned()),
            PresetCommand::SaveAs("New".to_owned()),
            PresetCommand::Overwrite,
            PresetCommand::Undo,
            PresetCommand::Rename("Other".to_owned()),
            PresetCommand::Delete,
        ];
        for skip in 0..all.len() {
            let cli = parse(&all[skip..]);
            let commands = cli.commands();
            assert!(
                commands.contains(&Command::Preset(expected[skip].clone())),
                "with {:?} on the line, {:?} should win",
                &all[skip..],
                expected[skip]
            );
            assert_eq!(
                commands
                    .iter()
                    .filter(|c| matches!(c, Command::Preset(_)))
                    .count(),
                1,
                "exactly one preset command may survive"
            );
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
    fn a_name_that_sanitises_away_drops_the_whole_preset_command() {
        // The `if`/`else if` chain has already matched by the time the name is checked
        // (`FxController.cpp:428-440`), so it must not fall through to `--delete_preset`.
        let commands = parse(&["--rename_preset=???", "--delete_preset"]).commands();
        assert!(
            !commands.iter().any(|c| matches!(c, Command::Preset(_))),
            "got {commands:?}"
        );
    }

    #[test]
    fn a_forwarded_command_line_shows_and_raises_the_window() {
        // Step 11 of applyConfig is an `else` (`FxController.cpp:523-531`).
        assert!(
            parse(&["--power=1"])
                .commands()
                .contains(&Command::Window(WindowCommand::Show))
        );
        assert!(
            parse(&[])
                .commands()
                .contains(&Command::Window(WindowCommand::Show))
        );
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
        // Mixed with an option the Windows build knows, the original behaviour comes back.
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
    fn cold_start_honours_only_the_options_init_config_reads() {
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

        // None of these exist in `initConfig` (`FxController.cpp:225-234`).
        let running_only = parse(&[
            "--overwrite_preset",
            "--set_band_gain=0:3",
            "--set_effect=bass:5",
        ]);
        assert!(running_only.cold_start_commands().is_empty());
        assert!(parse(&["--status"]).cold_start_commands().is_empty());
        // `--quit` with no instance to quit is decided in `main` before the engine starts; it
        // must not slip through here and start one.
        assert!(parse(&["--quit"]).cold_start_commands().is_empty());
    }

    #[test]
    fn input_names_a_microphone_and_off_detaches_either_lane() {
        assert_eq!(
            parse(&["--input=alsa_input.usb-fifine"]).commands(),
            vec![
                Command::Input(DeviceCommand::Select("alsa_input.usb-fifine".to_owned())),
                Command::Window(WindowCommand::Show),
            ]
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
                Command::Window(WindowCommand::Show),
            ]
        );
    }

    #[test]
    fn the_new_hotkey_shaped_options_leave_the_window_alone_and_the_device_ones_raise_it() {
        for hotkey in [
            &["--next-input"][..],
            &["--noise-suppression=off"][..],
            &[
                "--next-input",
                "--next-output",
                "--noise-suppression=strong",
            ][..],
        ] {
            let commands = parse(hotkey).commands();
            assert!(
                !commands.iter().any(|c| matches!(c, Command::Window(_))),
                "{hotkey:?} must not raise the window: {commands:?}"
            );
        }
        for raising in [
            &["--input=Mic"][..],
            &["--edit=input"][..],
            &["--output=off"][..],
            &["--next-input", "--input=Mic"][..],
            &["--noise-suppression=off", "--edit=input"][..],
        ] {
            assert!(
                parse(raising)
                    .commands()
                    .contains(&Command::Window(WindowCommand::Show)),
                "{raising:?} should raise the window"
            );
        }
    }

    #[test]
    fn cold_start_honours_the_lane_options_but_not_cycling_or_questions() {
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
}
