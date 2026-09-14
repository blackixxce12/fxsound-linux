# 00 — Master architecture for the Rust/egui/PipeWire port

**Status: normative.** This document is the single source of truth for how the FxSound Linux port
is assembled. Specs `01`–`13` describe *what the Windows application does*; this document decides
*what we build*, in what order, and with which exact types. Where a sibling spec offers options,
this document picks one and the implementation follows it without re-litigating.

Every claim about existing code is cited as `path:line` relative to
`/home/blackixxce/Загрузки/fxsound-app-main`. Every claim about a third-party API is cited against
the verified cheatsheets in `docs/api/`, which were compiled against the exact pinned versions.

---

## 1. Ground rules

1. **The RT audio thread never allocates, locks, blocks, logs, or panics.** This is the one rule
   that outranks fidelity to the original. The C++ engine violates it ~35 times per buffer by
   reading the Windows registry from `processAudio` (`docs/spec/08-dsp-api.md:1157-1186`); the port
   does not.
2. **Nothing mutates global audio state without a save/restore contract.** Only one thing here is
   global: `default.configured.audio.sink`. It is read and persisted before the first write and
   restored on clean exit, `SIGINT`, `SIGTERM` and panic
   (`docs/spec/12-audio-io.md:1361-1394`).
3. **Pixel geometry, palette, preset bytes, settings key names and DSP constants are frozen
   contracts.** They live in `fxsound-core` and `fxsound-ui::{theme,layout}` and are already
   written and tested. Nothing downstream re-derives them.
4. **Windows quirks are ported only when they are audible or visible.** Documented bugs
   (the Lite error-toast at x = −50, the 8 px slider-fill overshoot, the 2 px settings-pane
   overhang, `isPowerOn()` being inverted) are **fixed**, and each fix is listed in §9.
5. **egui 0.36 / eframe 0.36 / pipewire 0.10 API from the cheatsheets, never from memory.** The
   three traps that will otherwise cost a day: `eframe::App`'s required method is
   `fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)` and there is **no `update`**
   (`docs/api/eframe-0.36.md:315`); `TopBottomPanel`/`SidePanel` do not exist, there is one `Panel`
   type taking an `Id` (`docs/api/eframe-0.36.md:20`); `Rounding` is `CornerRadius` with `u8` fields
   (`docs/api/egui-0.36-painting.md:39-42`).

---

## 2. Crate layout

### 2.1 As it stands

`fxsound-linux/Cargo.toml` is a `resolver = "3"`, `edition = "2024"`, `rust-version = "1.98.1"`
workspace with six members. Pinned versions, verbatim from the workspace manifest:

| Dependency | Pin | Used by |
|---|---|---|
| `egui` | `=0.36.0` | fxsound-ui |
| `eframe` | `=0.36.0`, `default-features = false`, `["default_fonts","glow","wayland","x11"]` | fxsound-app |
| `egui_extras` | `=0.36.0`, `default-features = false` | **unused — delete** (see §2.3) |
| `pipewire` / `libspa` | `0.10.1` | fxsound-audio (with `features = ["v0_3_65"]` set per-crate) |
| `resvg` / `usvg` / `tiny-skia` | `0.48.1` / `0.48.1` / `0.12.0` | fxsound-ui |
| `ksni` | `0.3.6` | fxsound-app |
| `notify-rust` | `4.18.0` | fxsound-app |
| `rfd` | `0.17.2` | fxsound-app |
| `crossbeam-channel` | `0.5.17` | control-thread messaging |
| `triple_buffer` | `9.0.0` | GUI↔RT parameter + meter transport |
| `arc-swap` | `1.9.2` | not needed — see §2.3 |
| `rtrb` | `0.3.5` | sink→output PCM ring, RT→GUI event ring |
| `realfft` | `3.5.0` | fxsound-dsp::spectrum |
| `serde` / `toml` / `dirs` | `1.0.229` / `1.1.6` / `7.0.0` | fxsound-core::settings |
| `anyhow` / `thiserror` / `log` / `env_logger` / `clap` | `1.0.104` / `2.0.20` / `0.4.34` / `0.11.11` / `4.6.6` | app-level |

Release profile is `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, **`panic = "abort"`** —
the last is load-bearing, because a panic unwinding through PipeWire's `extern "C"` trampoline is
undefined behaviour (`docs/api/pipewire-0.10-rust.md:2338`, `:2408`).

```
fxsound-linux/
├── Cargo.toml                     workspace, resolver "3", edition 2024, rust 1.98.1
├── assets/{fonts,images,presets}/  embedded at build time
├── packaging/                      .desktop ×2, systemd user unit, hyprland.conf.example
├── docs/{spec,api}/
└── crates/
    ├── fxsound-core     no deps on us; the vocabulary                 [WRITTEN]
    ├── fxsound-dsp      core                                          [WRITTEN except leveling + ambience]
    ├── fxsound-preset   core                                          [WRITTEN]
    ├── fxsound-audio    core + dsp + pipewire                         [EMPTY]
    ├── fxsound-ui       core + egui + resvg                           [theme/layout/assets/state/slider WRITTEN]
    └── fxsound-app      all of the above + eframe/ksni/notify-rust/rfd/clap   [EMPTY]
```

Dependency edges are acyclic and deliberately narrow: **`fxsound-ui` must never depend on
`fxsound-audio`, `fxsound-preset` or `pipewire`.** It renders a `UiState` and returns a
`UiResponse`; that is what makes it testable headless. **`fxsound-dsp` must never depend on
`pipewire`.** It is a pure `&mut [f32]` transformer.

### 2.2 What is already written and is a fixed contract

| File | Lines | Contract it freezes |
|---|---:|---|
| `crates/fxsound-core/src/lib.rs` | 360 | `Effect` (GUI order 0..4), the three index orderings (`vals_index`, `app_depend_index`), `scale::{midi_to_value,value_to_midi,value_to_slider,slider_to_value}`, `EqBand`, `eq::{MAX_BANDS=32,DEFAULT_BANDS=10,MAX_GAIN_DB=12.0,DEFAULT_CENTERS_HZ}`, `Preset`, `AudioDevice`, `AudioStatus`, `NUM_SPECTRUM_BARS=10`, `SpectrumFrame` |
| `crates/fxsound-core/src/messages.rs` | 171 | `DspParams` (`Copy`, no heap), `DspEvent`, `Meters`, `UiToAudio`, `AudioToUi` |
| `crates/fxsound-core/src/settings.rs` | 294 | `Settings` with the original Windows key names, `Hotkeys`, `DeviceConfig`, `ViewMode`, `ThemeMode`, XDG paths, atomic save |
| `crates/fxsound-ui/src/theme.rs` | 370 | `FxColor` (27 ids), `DARK`/`LIGHT` tables transcribed from `FxTheme.cpp:22-29`, `Palette::{color,color_alpha,visuals}`, Gilroy + Noto font registration, `fonts::{REGULAR,SEMIBOLD,BOLD}` |
| `crates/fxsound-ui/src/layout.rs` | 332 | every pixel constant: `WINDOW_CORNER_RADIUS=21`, `TITLE_BAR_HEIGHT=56`, `Chrome::{PRO,LITE}` button rects, `pro::*` / `lite::*` rects, `audio_controls`, `equalizer`, `visualizer`, `settings_dialog` |
| `crates/fxsound-ui/src/state.rs` | 308 | `UiState`, `UiAction` (27 variants), `UiResponse`, `PresetEntry` |
| `crates/fxsound-ui/src/assets.rs` | 399 | `FxImage` (32 slots), `svg_bytes`, `rasterise`, `tinted`, `AssetCache` |
| `crates/fxsound-ui/src/widgets/slider.rs` | 395 | `FxSlider`, `THUMB_RADIUS=8.0`, `track_rect`, `quantise` |
| `crates/fxsound-dsp/src/{biquad,eq,spectrum,engine}.rs` + `effects/*` | 4 022 | `Engine`, `GraphicEq`, `SpectrumAnalyser`, `Chain`, `Effect` trait, five effect structs |
| `crates/fxsound-preset/src/{lib,store}.rs` | 924 | `.fac` parse/write, `PresetStore`, autosave semantics |

**Do not change any public item in the table above without updating this document.**

### 2.3 Cargo.toml deltas required before Phase 1

```toml
# crates/fxsound-audio/Cargo.toml — the v0_3_65 features are already right; keep that comment.
# ADD exactly one dependency:
rtrb = { workspace = true }
# Do NOT add fxsound-preset here: the audio crate never touches files.

# crates/fxsound-ui/Cargo.toml — NO CHANGES.
# fxsound-ui must not depend on fxsound-preset, fxsound-audio or pipewire. It receives
# `state::PresetEntry { name, factory, modified }` from the app and knows nothing about paths.

# crates/fxsound-app/Cargo.toml — currently only fxsound-core. Needs:
fxsound-core, fxsound-dsp, fxsound-preset, fxsound-audio, fxsound-ui   (workspace = true)
eframe, egui, ksni (+ feature "blocking"), notify-rust, rfd,
crossbeam-channel, triple_buffer, anyhow, thiserror, log, env_logger, clap, dirs

# workspace [dependencies] — remove:
egui_extras   # default-features = false compiles NO loaders and NO svg
              # (docs/api/linux-desktop-crates.md:75-77); we rasterise with resvg ourselves.
arc-swap      # superseded: DspParams is Copy and goes through triple_buffer.
              # Keep only if a future non-Copy coefficient block appears.

# ksni must gain the blocking feature so the tray can live on a plain std::thread
# without an app-wide tokio runtime (docs/api/linux-desktop-crates.md:166-182):
ksni = { version = "0.3.6", features = ["blocking"] }
```

---

## 3. Public API each remaining crate must expose

Signatures below are normative. Where a type already exists it is named, not re-declared.

### 3.1 `fxsound-dsp` — the two modules still missing

```rust
// crates/fxsound-dsp/src/leveling.rs   (ports SosProcess.cpp:38-76 constants, :139-472 algorithm)
pub struct VolumeLeveling { /* fixed-size: power_history[6], quiet_peak_history[30], per-ch state */ }

impl VolumeLeveling {
    /// Allocation-free after this call. `max_channels` is the worst case (8).
    pub fn new(sample_rate: Real, max_channels: usize) -> Self;
    pub fn set_sample_rate(&mut self, sample_rate: Real);
    /// `amount` is the abstract 0.0..=4.0 slider; 0.0 disables and resets the state machine
    /// (`GraphicEqSet.cpp:83-87`, `SosSet.cpp:292-320`).
    pub fn set_amount(&mut self, amount: Real);
    pub fn reset(&mut self);
    /// RT-safe. Ramps gain linearly across the buffer (`SosProcess.cpp:376-379`) and hard-clips
    /// to `effective_ceiling` (`:693-697`).
    pub fn process(&mut self, buffer: &mut [Real], channels: usize);
}
```

```rust
// crates/fxsound-dsp/src/effects/ambience.rs   REPLACES the current pass-through stub
// Dattorro figure-of-eight plate (docs/spec/10-dsp-effects.md:736-1090).
pub struct Ambience { /* one contiguous ring of DSPS_SOFT_MEM_LEX_LENGTH = 140_370 f32 */ }

impl Ambience {
    pub fn new(sample_rate: Real) -> Self;   // allocates the tank ONCE, here
}
impl Effect for Ambience { /* set_sample_rate, set_amount, amount, is_active, reset, process, latency_frames */ }
```

`Ambience::is_active()` must return `false` when the MIDI-equivalent amount is
`<= DFXP_MIN_EFFECTIVE_MIDI_AMBIENCE (12)` — i.e. slider ≤ 0.99 is genuinely silent
(`dfxpComm.cpp:1688`). Under the shipped `MUSIC2` mode the knob is additionally scaled by
**0.34** (`dfxpDefs.h:128`); that scaling belongs inside `set_amount`, not at the UI edge.

Everything else in `fxsound-dsp` is written. `Engine`'s contract, already on disk:

```rust
pub fn new(sample_rate: f32, max_block_frames: usize, channels: usize) -> Self;  // NOT RT-safe
pub fn set_format(&mut self, sample_rate: f32, channels: usize);                  // NOT RT-safe
pub fn apply(&mut self, params: &DspParams);                                      // RT-safe
pub fn handle_event(&mut self, event: DspEvent);                                  // RT-safe
pub fn process(&mut self, buffer: &mut [f32], channels: usize);                   // RT-safe
pub fn meters(&self) -> Meters;                                                   // RT-safe
pub fn reset(&mut self);  pub fn latency_frames(&self) -> usize;
```

`Engine::new` is called with `max_block_frames = 2048` and `channels = 8` — the worst case from
§6.2 — so `set_format` never needs to grow a buffer.

### 3.2 `fxsound-audio` — the whole crate

```rust
// crates/fxsound-audio/src/lib.rs
#![forbid(unsafe_code)]            // except ring.rs, which needs none with rtrb
pub mod device;    pub mod error;   pub mod rules;
pub mod settings;  pub mod engine;  mod pw;   mod ring;

pub use device::{FormFactor, SoundDevice};
pub use engine::{AudioConfig, AudioEngine, Quality};
pub use error::AudioError;
pub use settings::AudioSettings;
```

```rust
// device.rs  — the Linux analogue of SoundDevice (AudioPassthru.h:32-53)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoundDevice {
    pub id: String,            // node.name — THE identity, persisted
    pub object_id: u32,        // runtime handle only
    pub object_serial: u64,
    pub friendly_name: String, // node.description
    pub description: String,   // node.nick, falling back to description
    pub form_factor: FormFactor,
    pub channels: u32,
    pub rate: u32,
    pub is_active: bool,
    pub is_default: bool,
    pub is_virtual_sink: bool,   // == our own fxsound_sink
    pub is_targeted_output: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormFactor {
    Speakers, Headphones, Headset, LineLevel, Spdif, Hdmi,
    DigitalPassthrough, NetworkDevice, Handset, Microphone, Unknown,
}
impl SoundDevice { pub fn to_core(&self) -> fxsound_core::AudioDevice; }
```

```rust
// error.rs — 1:1 with the states the GUI already knows how to render
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no output devices present")]            NoOutputDevices,      // ≡ 209
    #[error("selected output is not present")]       DeviceNotPresent,     // ≡ -2
    #[error("output device is unavailable")]         DeviceUnavailable,    // ≡ -54
    #[error("no usable (stereo or better) output")]  NoValidOutput,        // ≡ -57
    #[error("please choose an output device")]       AskUserSelectOutput,  // ≡ -58
    #[error("PipeWire is not available: {0}")]       PipewireUnavailable(String),
    #[error("lost connection to PipeWire")]          PipewireDisconnected,
    #[error("format negotiation failed")]            FormatNegotiation,    // ≡ -35/-36
}
```

```rust
// rules.rs — pure, unit-testable, no PipeWire types. Port of sndDevicesImplementDeviceRules.
pub struct RuleInput<'a> {
    pub sinks: &'a [SoundDevice],
    pub current_default: Option<&'a str>,
    pub our_sink_name: &'a str,
    pub previous_sinks: &'a [String],
    pub settings: &'a AudioSettings,
}
pub enum RuleOutcome {
    Target { id: String, write_prev_default: bool },
    Error(AudioError),
}
pub fn choose_output(input: RuleInput<'_>) -> RuleOutcome;
```

```rust
// settings.rs — $XDG_CONFIG_HOME/fxsound/audio.toml, separate from settings.toml
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub schema: u32,                     // 1
    pub original_default: String,
    pub most_recent_default: String,
    pub prior_default: String,
    pub most_recent_playback: String,
    pub user_selected_playback: String,  // WE WRITE THIS, unlike the Windows build
    pub auto_default_mode: bool,         // true
    pub quality: Quality,                // Normal
}
impl AudioSettings { pub fn load() -> Self; pub fn save(&self) -> std::io::Result<()>; }
```

```rust
// engine.rs — everything the GUI thread touches
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality { Low, #[default] Normal, Safe, Max }
impl Quality { pub const fn quantum(self) -> u32; }   // 256 / 512 / 1024 / 2048

pub struct AudioConfig {
    pub sink_node_name: String,       // "fxsound_sink"
    pub sink_description: String,     // "FxSound"
    pub quality: Quality,
    pub take_default_sink: bool,
    pub initial_params: DspParams,
}

pub struct AudioEngine { /* owns the PipeWire thread's JoinHandle and every endpoint below */ }

impl AudioEngine {
    /// Spawns the PipeWire thread. Returns as soon as the thread is up; readiness and errors
    /// arrive on `events()`. NOT called from the RT thread.
    pub fn spawn(config: AudioConfig) -> Result<Self, AudioError>;

    /// Publish a complete parameter snapshot. Wait-free, always-latest-wins.
    pub fn publish(&mut self, params: &DspParams);
    /// Queue a must-not-coalesce event. Returns false if the (bounded) ring is full.
    pub fn send_event(&mut self, event: DspEvent) -> bool;
    /// Latest meters, for the visualizer. Wait-free read.
    pub fn meters(&mut self) -> Meters;
    /// Frames the engine processed while un-bypassed. Replaces getTotalAudioProcessedTime().
    pub fn frames_processed(&self) -> u64;

    /// Blocking-free control channel: device selection, rescan, restart, shutdown.
    pub fn control(&self) -> &crossbeam_channel::Sender<UiToAudio>;
    /// Device lists, status, disconnects, errors. Drain non-blockingly each GUI frame.
    pub fn events(&self) -> &crossbeam_channel::Receiver<AudioToUi>;

    /// Restore the pre-launch default sink, tear both nodes down, join the thread.
    /// Idempotent; also invoked from `Drop` and from the signal handler.
    pub fn shutdown(&mut self);
    pub fn xruns(&self) -> (u64, u64);   // (underruns, overruns)
}
impl Drop for AudioEngine { fn drop(&mut self) { self.shutdown(); } }
```

`AudioEngine` must be constructible **before** PipeWire has any sinks — node 1 is created
regardless, node 2 connects when a target appears (`docs/spec/12-audio-io.md:1740-1744`).

### 3.3 `fxsound-ui` — the modules still missing

```rust
// crates/fxsound-ui/src/lib.rs  (extend the existing pub mod list)
pub mod chrome;    // FxWindow: frameless frame, title bar, close glyph, drag region
pub mod view;      // pro, lite, settings, preset_io
pub mod i18n;      // t!(key) shim; the catalogue itself is owned by fxsound-app

// chrome.rs
pub struct ChromeResponse { pub actions: Vec<UiAction>, pub drag_started: bool }
/// Paints the rounded window, the 1 px divider at y = TITLE_BAR_HEIGHT, the wordmark with its
/// 600 ms processing cross-fade, and the six chrome buttons at layout::Chrome::{PRO,LITE}.
pub fn window_frame(
    ui: &mut Ui, state: &UiState, palette: Palette, assets: &mut AssetCache,
    chrome: layout::Chrome, logo_fade: f32,
) -> ChromeResponse;

// view/mod.rs
pub fn pro(ui: &mut Ui, state: &mut UiState, palette: Palette, assets: &mut AssetCache) -> UiResponse;
pub fn lite(ui: &mut Ui, state: &mut UiState, palette: Palette, assets: &mut AssetCache) -> UiResponse;

// view/settings.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsTab { #[default] Audio, General, Help }
pub struct SettingsState {
    pub tab: SettingsTab,
    pub devices: Vec<AudioDevice>,      // priority order
    pub device_presets: Vec<String>,
    pub prioritize_new_output: bool,
    pub hide_help_tooltips: bool,
    pub hide_notifications: bool,
    pub hotkeys_enabled: bool,
    pub hotkey_bindings: fxsound_core::settings::Hotkeys,
    pub launch_on_startup: bool,
    pub language: String,
    pub version: &'static str,
}
pub enum SettingsAction {
    Close, SetTab(SettingsTab), MoveDeviceUp(usize), MoveDeviceDown(usize),
    RemoveDevice(usize), SetDevicePreset(usize, String),
    SetPrioritizeNewOutput(bool), SetHideTooltips(bool), SetHideNotifications(bool),
    SetHotkeysEnabled(bool), SetLaunchOnStartup(bool), SetLanguage(String),
    ResetPresetsRequested, OpenUrl(&'static str),
}
pub fn settings(ui: &mut Ui, s: &mut SettingsState, palette: Palette, assets: &mut AssetCache)
    -> Vec<SettingsAction>;
```

New widgets (all in `crates/fxsound-ui/src/widgets/`, all `pub`):

```rust
pub struct FxComboBox<'a> { /* label, items, selected, enabled, error */ }
impl<'a> FxComboBox<'a> {
    pub fn new(selected: &'a mut Option<usize>, items: &'a [String]) -> Self;
    pub fn enabled(self, b: bool) -> Self;
    pub fn error(self, b: bool) -> Self;
    pub fn separator_after(self, index: usize) -> Self;   // the AppPreset→UserPreset rule
    pub fn show(self, ui: &mut Ui, rect: Rect, palette: Palette, assets: &mut AssetCache,
                id_salt: impl Hash + Debug) -> Response;
}

pub struct FxVerticalSlider<'a> { /* EQ band gain, -12..=+12 dB step 1 */ }
pub struct FxRotary<'a>        { /* EQ band frequency wheel, 300° sweep from 210° */ }
pub struct FxBalanceSlider<'a> { /* the two-sided gradient track */ }
pub struct FxPowerButton       { /* 24×24, two-state, 50 % alpha when disabled */ }
pub struct FxImageButton       { /* normal + hover SVG pair, exact rect, PointingHand */ }
pub struct FxPresetNameEditor<'a> { /* 200×30, 64-char cap, 3-state validation border */ }
pub struct FxLanguageSwitcher<'a> { /* 180×30, r=5, prev/next chevrons, native name */ }

pub fn visualizer(ui: &mut Ui, rect: Rect, graph: &[f32; 100], palette: Palette,
                  enabled: bool, processing: bool);
pub fn equalizer(ui: &mut Ui, rect: Rect, state: &mut UiState, palette: Palette,
                 assets: &mut AssetCache) -> Vec<UiAction>;
pub fn confirm_modal(ctx: &egui::Context, text: &str, style: ConfirmStyle) -> Option<bool>;
pub fn toast(ui: &mut Ui, rect: Rect, text: &str, link: Option<(&str, &str)>, palette: Palette);
```

### 3.4 `fxsound-app` — the binary

```rust
crates/fxsound-app/src/
  main.rs         // clap parse -> single-instance probe -> primary or client -> run_native
  cli.rs          // the option table; init_config() and apply_config()
  app.rs          // struct FxApp; impl eframe::App
  controller.rs   // the port of FxController: preset lifecycle, power, device selection, autosave
  ipc.rs          // abstract AF_UNIX SOCK_SEQPACKET + flock, JSON frames, SO_PEERCRED check
  tray.rs         // ksni::Tray impl + Handle updates
  notify.rs       // org.freedesktop.Notifications via notify-rust, single replaces_id
  dialogs.rs      // rfd folder/file pickers on a worker thread
  autostart.rs    // ~/.config/autostart/com.fxsound.FxSound.desktop
  i18n.rs         // catalogue load, language switch, t!()
  status.rs       // status.json writer + socket reply payload
  signals.rs      // SIGINT/SIGTERM -> graceful shutdown (restore default sink first)
```

```rust
// app.rs — the only eframe::App in the process
pub struct FxApp {
    ui:        UiState,                   // fxsound-ui::state
    settings:  Settings,                  // fxsound-core::settings
    controller: Controller,               // preset store, dirty flags, autosave counter
    audio:     AudioEngine,
    params:    DspParams,                 // authoritative; published on change
    assets:    AssetCache,
    palette:   Palette,
    visual:    [f32; 100],                // the mirrored 10×10 bar history
    from_desktop: crossbeam_channel::Receiver<DesktopEvent>,
    to_desktop:   crossbeam_channel::Sender<DesktopCommand>,
    tray:      ksni::Handle<FxTray>,
    last_tick: std::time::Instant,
    skip_ticks: u8,
    logo_fade: f32,
}

impl eframe::App for FxApp {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame);  // 100 ms tick, tray sync
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame);       // REQUIRED
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::TRANSPARENT.to_array()                                // frameless + rounded
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>);           // glow feature is on
}
```

`fn logic` is the right home for the 100 ms heartbeat: eframe calls it **even while the window is
hidden or minimised**, when no egui pass runs at all (`docs/api/eframe-0.36.md:307-311`). That is
exactly the "tray-only, still processing audio" state the Windows build runs in.

---

## 4. Thread architecture

```
 ┌───────────────────────────────────────────────────────────────────────────────────┐
 │ T1  GUI / winit event loop            eframe::run_native, ViewportId::ROOT        │
 │     owns: FxApp, UiState, Settings, Controller, DspParams, AssetCache, Palette    │
 │     wakes on: input, ctx.request_repaint_after(100 ms), ctx.request_repaint()     │
 │     from any producer thread (EventLoopProxy, docs/api/eframe-0.36.md:784-795)    │
 └──┬────────────┬──────────────────┬─────────────────────────────┬──────────────────┘
    │ params_tx  │ events_tx        │ control_tx                  ▲ ui_rx
    │ triple_buf │ rtrb SPSC        │ crossbeam bounded(64)       │ crossbeam unbounded
    │ <DspParams>│ <DspEvent>       │ <UiToAudio>                 │ <AudioToUi|DesktopEvent>
    ▼            ▼                  ▼                             │
 ┌─────────────────────────────┐  ┌──────────────────────────────┴──────────────────┐
 │ T2  PipeWire DATA thread    │  │ T3  PipeWire MAIN-LOOP thread                   │
 │     (RT, SCHED_FIFO 83-88,  │  │     MainLoopRc::run(), thread-affine            │
 │      created by libpipewire)│  │     owns: ContextRc, CoreRc, RegistryRc,        │
 │                             │  │           Metadata proxy, 2 × StreamRc          │
 │  sink.process():            │  │     drains control_tx via pipewire::channel     │
 │    dequeue -> Engine::apply │  │     registry global/global_remove -> SoundDevice │
 │           -> Engine::process│  │     core error -> reconnect FSM (200 ms..5 s)    │
 │           -> ring.push      │  └─────────────────────────────────────────────────┘
 │    meters_tx.write(...)     │
 │    frames.fetch_add(...)    │      ┌──────────────────────────────────────────────┐
 │                             │      │ T4  desktop thread (std::thread, NOT tokio)  │
 │  out.process():             │      │     ksni::blocking::TrayMethods::spawn()     │
 │    ring.pop or zero-fill    │      │     notify-rust .show()  (zbus::block_on)    │
 └─────────────────────────────┘      │     rfd::FileDialog on demand                │
                                      └──────────────────────────────────────────────┘
 ┌───────────────────────────────────────────────────────────────────────────────────┐
 │ T5  ipc thread   accept() on the abstract AF_UNIX SOCK_SEQPACKET socket,          │
 │                  SO_PEERCRED == geteuid(), one JSON frame per datagram            │
 └───────────────────────────────────────────────────────────────────────────────────┘
```

### 4.1 Every channel, named

| Endpoint pair | Crate + type | Direction | Bound | Purpose |
|---|---|---|---|---|
| `params_tx` / `params_rx` | `triple_buffer::Input<DspParams>` / `Output<DspParams>` | T1 → T2 | 3 slots, wait-free | Whole-snapshot parameter publish. Coalescing is correct: parameters are state, not deltas (`crates/fxsound-core/src/messages.rs:9-12`). |
| `events_tx` / `events_rx` | `rtrb::Producer<DspEvent>` / `Consumer<DspEvent>` | T1 → T2 | 64 | `ResetFilterState`, `ResetSpectrum`, `ResetProcessedTime` — must not coalesce. |
| `meters_tx` / `meters_rx` | `triple_buffer::Input<Meters>` / `Output<Meters>` | T2 → T1 | 3 slots | Spectrum + peaks + sample rate + active flag. |
| `frames` | `Arc<AtomicU64>` | T2 → T1 | — | Frames processed while un-bypassed. Replaces the truncating millisecond counter (`dfxpUniversal.cpp:353`); convert to ms only on read. |
| `xruns` | `Arc<AtomicU64>` ×2 | T2 → T1 | — | underruns, overruns. |
| `pcm` | `rtrb::Producer<f32>` / `Consumer<f32>` | T2(sink) → T2(out) | `8 × 2048 × 8` f32 = 512 KiB | The only data path between the two `process()` callbacks. Target fill `1.5 × quantum`. |
| `control_tx` / `control_rx` | `crossbeam_channel::Sender<UiToAudio>` → `pipewire::channel::Receiver` | T1 → T3 | 64, `try_send` | `SelectOutput`, `RescanDevices`, `SetAsDefaultSink`, `Restart`, `Shutdown`. |
| `ui_tx` / `ui_rx` | `crossbeam_channel::{Sender,Receiver}<AudioToUi>` | T3 → T1 | unbounded | `Devices`, `Status`, `Disconnected`, `Error`. Producer calls `egui_ctx.request_repaint()` after `send`. |
| `desk_tx` / `desk_rx` | `crossbeam_channel::{Sender,Receiver}<DesktopEvent>` | T4/T5 → T1 | unbounded | Tray clicks, notification actions, CLI frames, file-dialog results. |
| `cmd_tx` / `cmd_rx` | `crossbeam_channel::{Sender,Receiver}<DesktopCommand>` | T1 → T4 | unbounded | `Notify{..}`, `PickFolder{..}`, `UpdateTray(TraySnapshot)`, `Quit`. |

Note the deliberate asymmetry: **T1 → T3 must not use `pipewire::channel::Sender` directly from the
RT thread** — it takes a `Mutex` and may `write(2)` (`docs/api/pipewire-0.10-rust.md:2342`). It is
fine from T1, which is what we do.

### 4.2 Thread-affinity constraints from the bindings

* **Nothing in `pipewire` or `libspa` is `Send` or `Sync`** — zero `unsafe impl Send` in the whole
  crate (`docs/api/pipewire-0.10-rust.md:38-41`). Every loop/context/core/stream/proxy lives and
  dies on T3. Cross-thread traffic goes through `pipewire::channel` only.
* **`Loop::add_signal_local` asserts the thread is literally named `"main"`**
  (`docs/api/pipewire-0.10-rust.md:43-46`). T3 is a spawned thread, so signal handling lives in
  `signals.rs` on the main thread, not in the PipeWire loop.
* Use the **`Rc` family** (`MainLoopRc`, `ContextRc`, `CoreRc`, `StreamRc`, `RegistryRc`) throughout
  T3, which removes the whole class of `StreamBox<'c>` drop-order bugs
  (`docs/api/pipewire-0.10-rust.md:2484-2489`).
* `ksni` `spawn()` on the tokio path requires being inside a tokio runtime; the `blocking` feature
  builds a private current-thread runtime instead (`docs/api/linux-desktop-crates.md:166-182`).
  T4 uses `ksni::blocking::TrayMethods` and is a plain `std::thread`.
* `notify_rust::Notification::show()` is `zbus::block_on` over a full connect+send
  (`docs/api/linux-desktop-crates.md:31`). It is called on T4 and **never** on T1 or from inside a
  ksni callback.

### 4.3 Forbidden on the RT thread (T2) — the normative list

Inside `sink.process()` and `out.process()`, on **every** path including error paths:

1. **No heap traffic.** No `Box::new`, `Vec::push`/`resize`/`with_capacity`, `String`, `format!`,
   `to_owned`, `collect`, `Arc`/`Rc` clone, `HashMap` insert. Every buffer is sized in
   `Engine::new` / `param_changed` for `MAX_QUANTUM × MAX_CHANNELS`.
2. **No locks.** No `Mutex`, `RwLock`, `RefCell`, `Condvar`, `channel::recv`, `park`,
   `thread::yield_now`. Permitted primitives: `triple_buffer`, `rtrb`, `Atomic*` with
   `Relaxed`/`Acquire`/`Release`.
3. **No syscalls or I/O.** No `std::fs`, no network, no `SystemTime`, no `println!`/`log::*`/`dbg!`.
   Errors become an `AtomicU64` counter or an `rtrb` code drained by T3.
4. **No panics.** No `unwrap`, `expect`, `assert!`, slice indexing that can be out of bounds, or
   debug-mode integer overflow. Use `let … else { return }` on every `dequeue_buffer()`,
   `datas_mut().first_mut()` and `data()`. `panic = "abort"` is already set, and unwinding across
   `extern "C"` is UB either way.
5. **No PipeWire calls other than the RT-safe set.** Allowed: `Stream::dequeue_buffer`, `Buffer`
   drop (queues it back), `Stream::time`, `Stream::flush`, `Stream::trigger_process`. Forbidden:
   `connect`, `disconnect`, `update_params`, `set_active`, `pipewire::channel::Sender::send`
   (`docs/api/pipewire-0.10-rust.md:2330-2351`).
6. **No reconfiguration.** A pending format change is latched by `param_changed` on T3; `process()`
   sees a flag and passes audio through untouched for that one buffer rather than waiting.
7. **Denormals handled explicitly.** Keep the original's bias constants — `SOS_FLOAT_BIAS = 1.0e-30`
   per biquad, `1.0e-5` in the spectrum filter, `1.0e-24` in the maximizer envelope. They are part
   of the sound at very low levels (`docs/spec/08-dsp-api.md:1510-1514`). Optionally also set
   FTZ/DAZ once when the data thread first calls us.
8. **Bounded time.** Work is `O(frames × channels × active_sections)` with no data-dependent
   unbounded loops.

CI enforces 1–4 with an allocator shim that aborts on any allocation on a thread named by
PipeWire's data-thread naming convention, run for 10 minutes under load (Phase 2 DoD).

---

## 5. Module-by-module port map

`[W]` = already written. `[P<n>]` = target phase. `—` = deliberately not ported.

| C++ source | Role | Rust target | Status |
|---|---|---|---|
| `fxsound/Source/Main.cpp` | JUCE app, init order, crash filter, single instance | `fxsound-app::main`, `::signals`, `::ipc` | P1/P4 |
| `fxsound/Source/MainComponent.{h,cpp}` | abandoned window design (`docs/spec/01-window-layout.md:35-58`) | — **dead code, do not port** | — |
| `GUI/FxWindow.{h,cpp}` | frameless chrome, title bar, drag, close glyph, shadow | `fxsound-ui::chrome` + `layout::{Chrome,ChromeButton}` | `layout` [W], painting P3 |
| `GUI/FxMainWindow.{h,cpp}` | concrete window, 6 title-bar buttons, hamburger menu, icons | `fxsound-ui::chrome` + `fxsound-app::app` (menu) | P3/P4 |
| `GUI/FxView.{h,cpp}` | preset + output combos, error notification | `fxsound-ui::view` shared helpers, `widgets::FxComboBox`, `widgets::toast` | P3 |
| `GUI/FxProView.{h,cpp}` | 1040×511 layout, panel, enablement | `fxsound-ui::view::pro`, `layout::pro` | `layout` [W], view P3 |
| `GUI/FxLiteView.{h,cpp}` | 550×112 layout | `fxsound-ui::view::lite`, `layout::lite` | `layout` [W], view P4 |
| `GUI/FxTheme.{h,cpp}` colours | 27 ids × 2 palettes | `fxsound-ui::theme::{FxColor,DARK,LIGHT,Palette}` | [W] |
| `GUI/FxTheme.{h,cpp}` fonts | 3 weight slots, per-language faces | `fxsound-ui::theme::{font_definitions,regular,semibold,bold}` | [W] |
| `GUI/FxTheme.{h,cpp}` images | 32 slots × 2 themes | `fxsound-ui::assets::{FxImage,svg_bytes,AssetCache}` | [W] |
| `GUI/FxTheme.{h,cpp}` LookAndFeel overrides | drawComboBox / LinearSlider / Rotary / PopupMenu / Tooltip | `fxsound-ui::widgets::*` (§6) | slider [W], rest P3 |
| `GUI/FxModel.{h,cpp}` | observable state + 9 events | `fxsound-ui::state::{UiState,UiAction,UiResponse}`; events deleted (immediate mode) | [W] |
| `GUI/FxController.{h,cpp}` preset lifecycle | select/save/rename/delete/undo/reset/autosave | `fxsound-app::controller` + `fxsound-preset::PresetStore` | store [W], controller P3 |
| `GUI/FxController.{h,cpp}` device logic | `initOutputs`, `selectProcessingOutput`, `updateOutputs`, priority sort | `fxsound-audio::rules` + `fxsound-app::controller` | P1 |
| `GUI/FxController.{h,cpp}` CLI | `initConfig`, `applyConfig`, `printStatus` | `fxsound-app::{cli,status}` | P1/P4 |
| `GUI/FxController.{h,cpp}` hotkeys | `RegisterHotKey` ×5 | `packaging/hyprland.conf.example` + `fxsound-app::ipc` | P4 |
| `GUI/FxController.{h,cpp}` timer | 100 ms tick, 5-tick debounce, 600-tick autosave | `FxApp::logic` | P3 |
| `GUI/FxAudioControls.{h,cpp}` | 168×257 two-faced card, 5 effect sliders, 4 level sliders, flip, restore | `fxsound-ui::view::controls`, `layout::audio_controls` | `layout` [W], view P3 |
| `GUI/FxAudioSlider.{h,cpp}` | value-label slider | `fxsound-ui::widgets::slider::FxSlider` | [W] |
| `GUI/FxBalanceSlider.{h,cpp}` | two-sided gradient track | `fxsound-ui::widgets::FxBalanceSlider` | P3 |
| `GUI/FxEqualizer.{h,cpp}` | band layout, curve, alt-solo, tooltips | `fxsound-ui::view::equalizer` + `widgets::{FxVerticalSlider,FxRotary}` | P3 |
| `GUI/FxVisualizer.{h,cpp}` | 960×120, 100 bars, mirrored history, gradient | `fxsound-ui::widgets::visualizer` | P3 |
| `GUI/FxPowerButton.{h,cpp}` | 24×24 two-state | `fxsound-ui::widgets::FxPowerButton` | P3 |
| `GUI/FxComboBox.{h,cpp}` | themed combo, error outline, lazy popup | `fxsound-ui::widgets::FxComboBox` | P3 |
| `GUI/FxHyperlink.{h,cpp}` | underlined link | `fxsound-ui::widgets::link` | P5 |
| `GUI/FxPresetNameEditor.{h,cpp}` + `FxPresetMenuItem` | 200×30, 64 chars, 3-state border | `fxsound-ui::widgets::FxPresetNameEditor` (one widget for Save **and** Rename) | P4 |
| `GUI/FxSettingsDialog.{h,cpp}` | 610×597, 3 panes | `fxsound-ui::view::settings` + a deferred viewport | P5 |
| `GUI/FxOutputPreference.{h,cpp}` | device priority list, per-device preset | `fxsound-ui::view::settings::outputs` | P5 |
| `GUI/FxHotkeyLabel.{h,cpp}` | in-place key capture | `fxsound-ui::view::settings::hotkeys` — **read-only display + "copy compositor snippet"** | P5 |
| `GUI/FxLanguage.{h,cpp}` | 180×30 cycler | `fxsound-ui::widgets::FxLanguageSwitcher` + `fxsound-app::i18n` | P5 |
| `GUI/FxPresetImportDialog.{h,cpp}` | embedded FileBrowser | `fxsound-app::dialogs` (rfd portal folder picker) + `view::preset_io` summary | P5 |
| `GUI/FxPresetExportDialog.{h,cpp}` | multi-select list + progress | `fxsound-ui::view::preset_io` + `fxsound-preset::PresetStore::export` | P5 |
| `GUI/FxMessage.h` `FxConfirmationMessage` | 450×142 Yes/No/OK | `fxsound-ui::widgets::confirm_modal` (`egui::Modal`) | P5 |
| `GUI/FxMessage.h/.cpp` `FxMessage` | link-carrying modal | — **dead code, no call sites** | — |
| `GUI/FxNotification.{h,cpp}` | 216×80…560×120 toast | `fxsound-app::notify` (org.freedesktop.Notifications) + `widgets::toast` for the in-window error banner only | P4 |
| `GUI/FxSystemTrayView.{h,cpp}` | Shell_NotifyIcon, 4 icon states, context menu | `fxsound-app::tray` (`ksni`) | P4 |
| `Utils/Settings/Settings.{h,cpp}` | JUCE PropertiesFile | `fxsound-core::settings::Settings` (TOML, same key names) | [W] |
| `Utils/Settings/DeviceConfig.{h,cpp}` | device_configs JSON | `fxsound-core::settings::DeviceConfig` + `fxsound-audio::rules` | core [W], rules P1 |
| `Utils/SysInfo/SysInfo.{h,cpp}` | `isRemoteSession`, `canSupportHotkeys` | — **deleted**; no RDP analogue on PipeWire (`docs/spec/05-controller-model.md:1533`) | — |
| `dsp/include/DfxDsp.h`, `DfxDsp.cpp`, `DfxDspPrivate.cpp` | public façade + pimpl | `fxsound-dsp::engine::Engine` | [W] |
| `dsp/DfxDspEq.cpp`, `DspUtil/GraphicEq/*` | band tables, Q derivation, remap | `fxsound-dsp::eq::GraphicEq` | [W] |
| `ptutil/Filt/FiltCalcBiqd.cpp`, `SOS/SosProcess.cpp` (sections) | parametric design + TDF-II runner | `fxsound-dsp::biquad::{calc_parametric,Section}` | [W] |
| `SOS/SosProcess.cpp:139-472` | the 38-constant volume leveller | `fxsound-dsp::leveling::VolumeLeveling` | **P2 — missing** |
| `SOS/SosProcess.cpp:677-723` | RMS normalisation | `fxsound-dsp::engine` (`normalization_db`; disabled at 0.0) | [W] |
| `ptechDsp/Aural/Aural032/Auralp32.c` | Fidelity exciter | `fxsound-dsp::effects::fidelity::Fidelity` | [W] |
| `ptechDsp/Lex/Lex32/Lex32.c` | Ambience plate reverb | `fxsound-dsp::effects::ambience::Ambience` | **P2 — stub on disk** |
| `ptechDsp/wide/Wide32/Wide32.c` | Surround widener | `fxsound-dsp::effects::surround::Surround` | [W] |
| `ptechDsp/Maximizer/Maxi32/Maxi32.c` | Dynamic Boost | `fxsound-dsp::effects::dynamic_boost::DynamicBoost` | [W] |
| `ptechDsp/Play/Play32/Play32.c:692-759` | inline Bass biquad | `fxsound-dsp::effects::bass::Bass` | [W] |
| `ptechDsp/Play/Play32/Play32.c` (chain order) | Fidelity→Ambience→Surround→Bass→DynamicBoost | `fxsound-dsp::effects::Chain` | [W] |
| `DspUtil/spectrum/*` | 10-band analyser | `fxsound-dsp::spectrum::SpectrumAnalyser` (**re-derived per rate**, FFT-based) | [W] |
| `ptechDsp/*16.c`, `wide/Wide16`, vocal reduction, 8-tap headphone delay, dither | compiled out or hard-wired off | — **not ported** | — |
| `dsp/DfxDspPreset.cpp`, `ptutil/VALS/Valsfile.cpp` | `.fac` read/write | `fxsound-preset::{parse,write,load,save}` | [W] |
| `ptutil/PRELST/Prelst.cpp` | numbered preset slots | — **legacy, not ported** (`docs/spec/11-preset-format.md:58-87`) | — |
| `dsp/DfxDspRegistry.cpp` | registry session value | — **never called** | — |
| `ptutil/COM/Comwave.cpp` decimator | zero-order-hold rate reduction above 48 kHz | — **not ported**; run at the native rate (`docs/spec/08-dsp-api.md:1548-1556`) | — |
| `audiopassthru/**` (all 12 `sndDevices*.cpp` + `AudioPassthru*`) | WASAPI loopback, device rules, volume mirror, restart FSM | `fxsound-audio::{pw,rules,device,engine,ring}` | **P1** |
| `docs/COMMAND_LINE_OPTIONS.md` | CLI contract | `fxsound-app::cli` | P1/P4 |

---

## 6. egui widget inventory

egui has no `LookAndFeel` hook, so every JUCE-themed control becomes a widget that paints itself
with `egui::Painter` and hit-tests with `Ui::interact`. That is also what lets us keep the
original's absolute pixel layout instead of reflowing it — `layout.rs` gives the exact `Rect`, the
widget fills it.

| JUCE control | Rust widget | Custom `Painter`? | Notes / exact geometry |
|---|---|---|---|
| `FxWindow` rounded frame + shadow | `chrome::window_frame` | **Yes** | `painter.rect(rect, CornerRadius::same(21), bg, Stroke::NONE, StrokeKind::Inside)` — note `Painter::rect` takes **5** args in 0.36 (`docs/api/egui-0.36-painting.md:53-58`). 1 px divider at `y = 56` in `ControlBackground`. |
| `FxWindow::TitleBar` drag region | `Ui::interact(bar_rect, id, Sense::click_and_drag())` | No | `ViewportCommand::StartDrag` on `drag_started()` **only** — sending it every frame re-arms the compositor grab and jitters (`docs/api/egui-0.36-viewport.md:1196-1200`). |
| `FxWindow::CloseButton` (procedural ✕) | `chrome::close_button` | **Yes** | Two `Shape::line_segment`s at `Stroke::new(1.2, ImageButton)` in a 15×15 box; hit rect widened to 24×24 via `ChromeButton::hit_rect(24.0)`. No hover state. |
| `DrawableButton(ImageFitted)` ×5 (menu, donate, resize/flip, minimize, power) | `widgets::FxImageButton` | No (textured) | `AssetCache::texture(ctx, image, theme, size_points)` → `Image::new(&tex).fit_to_exact_size(..)`; hover swaps to the `*Hover` slot. `egui::ImageButton` is **gone** in 0.36 (`docs/api/egui-0.36-widgets-input.md:33`). |
| `FxPowerButton` | `widgets::FxPowerButton` | No (textured) | 24×24, `power_on`/`power_off` slot by state, alpha 0.5 when disabled, Space key activates. |
| `FxComboBox` (preset / output / EQ-bands) | `widgets::FxComboBox` | **Yes** | `corner_radius = height/5` (8 in Pro, 10 in Lite, 4 for the 20 px EQ-bands box), fill `ComboBoxBackground`, focus outline `SliderHighlight @ 0.2`, arrow box `(w-32, 0, 12, h)`, text clipped to `x+5 .. w-37`. Popup via `egui::Popup` anchored to the response rect; stock `egui::ComboBox` cannot express any of this. |
| `Slider::LinearHorizontal` ×9 | `widgets::slider::FxSlider` **[W]** | **Yes** | 160×18 component, track `(8, 7, 112, 3)` `CornerRadius::same(6)`, thumb 16×16 at `pos`, focus halo `SliderHighlight @ 0.1` expanded 4 px with radius 26. Fill bug fixed: `pos - rect.left()`, not `pos`. |
| `Slider::LinearVertical` ×N (EQ band gain) | `widgets::FxVerticalSlider` | **Yes** | 32×180 (or 32×216 for N>10) column, dashed rail `{5 on, 2 off}` 1 px, gradient `SliderTrack@0.4 → VerticalSliderLow@0.4`, region `y ∈ [24, 24+region_size]`, 1 dB grid, right-click → 0 dB. |
| `Slider::Rotary` (EQ band frequency) | `widgets::FxRotary` | **Yes** | 36×36 box reduced by 2, `arcRadius = 13.5`, `lineW = 5.0`, sweep 210°→510° (300°). `Shape::Path` has no cap style — add `circle_filled(end, 2.5)` at both ends to fake `PathStrokeType::rounded`. Hidden when N > 10, inert when N ≥ 15. |
| `FxBalanceSlider` | `widgets::FxBalanceSlider` | **Yes** | One gradient bar; `left = SliderTrack.withAlpha(1-t)`, `right = SliderTrack.withAlpha(t)`, `t = (v+20)/40`. Use `Shape::gradient_rect(rect, Direction::LeftToRight, [c0, c1])` — it exists in 0.36, do not hand-roll a mesh (`docs/api/egui-0.36-painting.md:71-72`). Gradient bug fixed: run it to `rect.right()`, not to `x = 112`. |
| `FxEqualizer` response curve | `widgets::equalizer` | **Yes** | Fill polygon as an `egui::Mesh` with per-vertex colours lerped between `EqStart@0.34` at `y=8` and `EqEnd@0.00` at `y=188/224`; polyline as per-segment `line_segment` at `Stroke::new(1.5, SliderTrack)`. Guard `N < 2` — the C++ indexes `band_boosts_[1]` unconditionally. |
| `FxVisualizer` | `widgets::visualizer` | **Yes** | One `Mesh`, 100 quads, `x0 = 27.0`, `dx = 9.1` **accumulated** (not multiplied), `w = 4.0`, `h = value*100`, centred on `y = 60`. Component-space gradient `GraphHigh → GraphLow@y=50 → GraphHigh@y=100`, alpha 1.0 processing / 0.75 idle. Value `0.0` is drawn as `0.01`. |
| `PopupMenu` (hamburger + submenus) | `egui::Popup` / `MenuBar` | Partly | `egui::menu::bar` is gone; `Popup::menu` / `Popup::context_menu` are the 0.36 API (`docs/api/egui-0.36-widgets-input.md:47`, `:975`). Ticked items get `rect_stroke(item_rect, CornerRadius::ZERO, Stroke::new(1.0, menu_text), StrokeKind::Inside)` after the label. |
| `PopupMenu::addCustomItem` (preset name field) | `widgets::FxPresetNameEditor` | **Yes** (2 px border) | `TextEdit::singleline` + `char_limit(64)` inside the popup; border `ValidTextBorder` when unique, `InvalidTextBorder` when empty or duplicate; Return commits, Escape cancels. Request focus **once on open**, not every frame. |
| `TooltipWindow` | `Response::on_hover_text` + a styled `Frame` | No | `menu_corner_radius = 5`, padding 10 h, wrap at 400 px, 14 px Semibold. The ±36/−18 px cursor offsets are not expressible — accept egui's placement. |
| `BubbleMessageComponent` (first-run help) | `egui::Area` + `Frame::popup` | No | timeout 0, dismiss on click; hand-rolled because `on_hover_ui` cannot express it. |
| `FxNotification` autohide toast | — | — | Replaced by `org.freedesktop.Notifications`. A Wayland client cannot place a surface at the tray corner. |
| `FxNotification` in-window error banner | `widgets::toast` | **Yes** | `egui::Area` at `Order::Foreground`, `CornerRadius::same(16)`, `Shadow { blur: 5, .. }`, `DefaultFill @ 1.0`, 560×120, **right-aligned to the output combo** (fixes the Lite x = −50 bug). |
| `FxSettingsDialog` tab button | `view::settings::tab_button` | **Yes** | 150×40, 40×40 rounded square `CornerRadius::same(10)` filled `MenuHighlightBackground`/`MenuBackground`, icon inset 10, label at `(45, 0, 115, 40)`. |
| `ListBox` (export list, device priority) | `egui::ScrollArea` + manual rows | Partly | Row heights 26 (export) / 40 (devices); selected row fills `ImageButton`. Salt each row's `Id` by a **stable device id**, not the row index, or popup state follows the wrong row after a reorder. |
| `ToggleButton` (6 checkboxes) | `egui::Checkbox` | No | JUCE's tick geometry is not in this tree (`docs/spec/06-dialogs.md:1376-1382`); use egui's and set `fg_stroke` to `HighlightedText`. |
| `FxHyperlink` | `widgets::link` | No | Underlined `DefaultText`, alpha ×0.4 when disabled. `eframe`'s `links` feature is **off**, so open URLs via `fxsound-app::dialogs::open_url` (portal `OpenURI`, else `xdg-open`), not `ui.hyperlink`. |
| `FxLanguage` switcher | `widgets::FxLanguageSwitcher` | **Yes** | 180×30 `CornerRadius::same(5)` `ControlBackground`, prev at `(10,4,14,22)`, next at `(156,4,14,22)`, centred native name in between. |
| `ScrollBar` | `egui::Style::spacing.scroll` | No | `bar_width = 10.0`, thumb `SliderTrack`. |

**Disabled state.** egui's built-in "greyed out" tint is a multiply toward the background; JUCE uses
`Colour::withSaturation(0.0)`, which is **`grey = max(r,g,b)`**, not a luma grey
(`docs/spec/02-theme.md:315-333`). `fxsound-ui::theme` must gain:

```rust
#[must_use] pub fn desaturate(c: Color32) -> Color32;   // grey = max(r,g,b), alpha preserved
```

and every custom painter calls it instead of relying on `ui.add_enabled_ui`.

---

## 7. Wayland / Hyprland specifics

### 7.1 Viewport configuration (root window)

```rust
let viewport = egui::ViewportBuilder::default()
    .with_title("FxSound")
    .with_app_id("com.fxsound.FxSound")        // MUST equal packaging/fxsound.desktop's filename stem
    .with_inner_size(layout::pro::WINDOW_SIZE)  // [1040.0, 588.0]
    .with_min_inner_size(layout::lite::WINDOW_SIZE)
    .with_resizable(false)                      // the original is not resizable at all
    .with_decorations(false)                    // we draw the title bar
    .with_transparent(true)                     // so the 21 px corners are not black
    .with_clamp_size_to_monitor_size(false);    // eframe defaults this to true on EVERY platform
                                                // (docs/api/egui-0.36-viewport.md:1553-1556)
let native_options = eframe::NativeOptions {
    viewport,
    renderer: eframe::Renderer::Glow,           // the ONLY variant that exists at our feature set
    run_and_return: false,
    centered: false,                            // "Wayland desktop currently not supported"
    ..Default::default()
};
```

* **`app_id` is set once and never changed** — changing it at runtime recreates the window
  (`docs/api/egui-0.36-viewport.md:1534-1535`).
* **Transparency needs two things**: `with_transparent(true)` *and* an `App::clear_color` that is
  actually transparent. The trait default is `rgba(12,12,12,180)`
  (`docs/api/eframe-0.36.md:357-365`).
* **CSD caveat.** eframe's `wayland` feature does **not** enable winit's `wayland-csd-adwaita` or
  `wayland-dlopen` (`docs/api/eframe-0.36.md:55`). We draw our own decorations, so that is exactly
  what we want; do **not** add those features back.
* **Fractional scaling** arrives through `wp_fractional_scale_v1` → winit → `ctx.pixels_per_point()`.
  Every constant in `layout.rs` is a **logical point at scale 1.0** and egui scales them. The one
  place scale must be handled by hand is SVG rasterisation, which `AssetCache::texture` already does
  by keying the cache on `(image, theme, round(size × ppp))`
  (`crates/fxsound-ui/src/assets.rs:245-252`).

### 7.2 View switching

Pro ⇄ Lite is `ctx.send_viewport_cmd(ViewportCommand::InnerSize(layout::{pro,lite}::WINDOW_SIZE))`.
On Linux the resize is applied immediately and **no `Resized` event may follow**; egui-winit assumes
it worked and refreshes the rects itself (`docs/api/egui-0.36-viewport.md:1361`). There is no
animation — the Windows build has none either.

> **Discrepancy to fix:** `packaging/hyprland.conf.example` says the sizes are `1040x511` and
> `570x162`. The contract in `crates/fxsound-ui/src/layout.rs:125,181` is `1040×588` and `550×189`
> (content + 56 title bar + 1 divider + 20 bottom pad). Update the packaging comment; `layout.rs`
> wins.

### 7.3 Secondary windows

Settings (610×597), Import and Export are **deferred viewports**, not `egui::Window`s:

```rust
ctx.show_viewport_deferred(
    egui::ViewportId::from_hash_of("fxsound.settings"),
    egui::ViewportBuilder::default()
        .with_title("Settings")
        .with_inner_size(layout::settings_dialog::WINDOW_SIZE)
        .with_resizable(false).with_decorations(false).with_transparent(true)
        .with_app_id("com.fxsound.FxSound"),
    move |ctx, _class| { /* chrome + pane */ },
);
```

They close by **not being shown next frame** — `ViewportCommand::Close` only pushes an event
(`docs/api/egui-0.36-viewport.md:1512-1515`). The blocking `runModalLoop()` shape of the C++ becomes
`enum DialogState { None, Settings, Import(ImportState), Export(ExportState), Confirm{..} }` on
`FxApp`. The small Yes/No/OK boxes stay in-window as `egui::Modal`.

### 7.4 Tray (StatusNotifierItem over D-Bus)

`ksni` 0.3.6 with the `blocking` feature, on T4. Mapping from `FxSystemTrayView`:

| Windows | ksni |
|---|---|
| 4 `HICON` states (gray/white/red/blue) | `Tray::icon_name()` → `com.fxsound.FxSound-{off,on,processing}`; ship a `-symbolic` variant and let the panel recolour. Keep `icon_pixmap()` as the fallback — note `ksni::Icon` is **ARGB32, network byte order**, not RGBA (`docs/api/linux-desktop-crates.md:29`). |
| `szTip` two-line tooltip | `ToolTip { title: "FxSound", description: "FxSound is on.\n\nOutput: <device>", .. }` — many panels render only `title`, so the important line goes first. |
| `NIN_SELECT` left click toggles the window | `Tray::activate(x, y)` → send `DesktopEvent::ToggleWindow` |
| `WM_CONTEXTMENU` | `Tray::menu() -> Vec<MenuItem<Self>>` |
| preset / device / theme radio ticks | `ksni::menu::RadioGroup` (three separate groups) |
| `Always On Top` check | `CheckmarkItem` — **greyed out on Wayland** (§7.6) |
| ≤ 5 devices inline, > 5 in a submenu | **Always a `SubMenu`.** DBusMenu has no section header and the inline special case only existed because Win32 menus are cheap. |
| 30-char device-name truncation | keep it; it is a real UX rule |
| `Shell_NotifyIcon(NIM_DELETE)` | drop the `ksni::Handle` |

Every callback must only `send` into `desk_tx`; blocking inside one freezes the menu
(`docs/api/linux-desktop-crates.md:28`). State changes flow the other way through
`Handle::update(|t| …)`, which is what actually emits the D-Bus property-changed signals.

**GNOME ships no SNI host by default.** If no watcher registers within 5 s, post a one-time
notification explaining it, and **refuse to start hidden** (`--hide` / `run_minimized`) when there is
neither a watcher nor a notification daemon — otherwise the app is invisible and unkillable from the
UI.

### 7.5 Notifications

`notify-rust` 4.18.0 on T4, one persistent `replaces_id` so a new message replaces the old one in
place — matching `FxModel`'s single-slot mailbox exactly:

```rust
Notification::new()
    .appname("FxSound").summary("FxSound").icon("com.fxsound.FxSound")
    .body(&text)                                     // \r\n → \n, at most 3 lines
    .hint(Hint::SuppressSound(true))                 // ≡ NIIF_NOSOUND
    .urgency(Urgency::Low)                           // ≡ NIIF_RESPECT_QUIET_TIME
    .hint(Hint::DesktopEntry("com.fxsound.FxSound".into()))
    .action("default", link_label)                   // only when a link exists
    .timeout(Timeout::Milliseconds(if has_link { 8000 } else { 7000 }))
    .id(replaces_id)
    .show()?;
```

`.actions(vec![..])` is deprecated — use `.action(id, label)`
(`docs/api/linux-desktop-crates.md:33`). Fix the Windows message-loss bug: `FxApp` keeps a
`VecDeque<Toast>` and drains one per 100 ms tick instead of `Thread::sleep(2000)`.

### 7.6 What cannot work as on Windows — and the Hyprland answer for each

| Windows behaviour | Wayland reality | Decision |
|---|---|---|
| **Global hotkeys** (`RegisterHotKey`, Ctrl+Shift+Q/E/A/Z/W) | A Wayland client cannot grab keys it does not have focus for. Full stop. | **Compositor binding → CLI → control socket.** `packaging/hyprland.conf.example` already ships `bind = CTRL SHIFT, F/D/P/O, exec, fxsound --next-preset/--prev-preset/--toggle-power/--toggle-window`. The settings pane shows the five actions **read-only** with a "copy Hyprland snippet" button. `org.freedesktop.portal.GlobalShortcuts` via `ashpd` is a **Phase 6+ optional**, gated on the portal existing; it is absent on most current setups. |
| **Always on top** | `ViewportCommand::WindowLevel` is unsupported on Wayland (`docs/api/egui-0.36-viewport.md:1340`). | **Hide the toggle on Wayland** in both menus; show it only when `ViewportInfo::outer_rect.is_some()` (our X11 proxy). The Help pane documents `windowrulev2 = pin, class:^(com\.fxsound\.FxSound)$`. |
| **Absolute window positioning** (`window_x`/`window_y`, `centreWithSize`) | `with_position` / `OuterPosition` are no-ops; `outer_rect` is always `None`. | **Dropped.** `Settings::{window_x,window_y}` stay in the schema (already documented as write-only, `crates/fxsound-core/src/settings.rs:105-108`) for an X11 fallback and for migration from a Windows settings file. Hyprland places the window; `packaging/hyprland.conf.example` already carries `float` + `center` rules. |
| **Lite view snapping to the tray corner** (`Shell_NotifyIconGetRect` + 10 px inset) | SNI has no "where is my icon" call, and a toplevel cannot place itself. | **Dropped.** `wlr-layer-shell` would work but needs a second, non-eframe windowing path and excludes GNOME. Document a Hyprland rule instead: `windowrulev2 = move 100%-570 60, class:^(com\.fxsound\.FxSound)$`. |
| **Hide to tray = destroy the window** (`removeFromDesktop`) | `ViewportCommand::Visible(false)` is unsupported on Wayland. | **Do not hide the root viewport.** `--hide` / `run_minimized` means: never create the root viewport at all, or close it and keep the process alive. With `eframe`, the practical shape is to keep the root viewport alive but `Minimized(true)`, and treat the tray's Open as re-showing. **Accept that un-minimize is unsupported on Wayland** (`docs/api/egui-0.36-viewport.md:1345`); the honest UX is that the close button and the minimise button both minimise, and the tray brings it back via `RequestUserAttention` where `xdg_activation_v1` exists. A hand-rolled `winit` `ApplicationHandler` that can run with zero windows is the correct long-term fix and is scheduled as **Phase 6, risk R-07**. |
| **Raise/focus on demand** (`SetForegroundWindow`) | `ViewportCommand::Focus` has no effect; `xdg-activation-v1` needs a token from a user action. | Send `Focus` anyway plus `RequestUserAttention(Informational)`. Document that `fxsound --toggle-window` may only flag the window on some compositors. |
| **Per-window icon** (`WM_SETICON`, 4 states) | `with_icon` is a Wayland no-op. | The taskbar icon comes from the `.desktop` matched by `app_id`. **Dynamic state lives in the tray icon and in the in-window wordmark cross-fade**, which the design already has. |
| **Single instance** (`anotherInstanceStarted`) | — | Abstract `AF_UNIX` `SOCK_SEQPACKET` at `\0fxsound/<uid>/<WAYLAND_DISPLAY>` + `flock` on `$XDG_RUNTIME_DIR/fxsound/instance.lock`; fall back to a filesystem socket in a sandbox. Pass **real argv**, never a re-joined string. Verify `SO_PEERCRED.uid == geteuid()`. |
| **Auto-update** (`updater.exe`) | Packages update apps on Linux. | **Deleted.** `Settings::automatic_updates` stays in the schema, inert, and the Help pane says so (already documented at `crates/fxsound-core/src/settings.rs:127-129`). |
| **Launch on startup** (HKCU\…\Run) | — | Exactly one file: `~/.config/autostart/com.fxsound.FxSound.desktop`. `packaging/fxsound-autostart.desktop` is the template. The systemd user unit is offered, never enabled alongside it. |
| **Remote-session lockout** | No RDP-equivalent constraint on a PipeWire sink. | **Deleted**, with it the four code paths it gated. |

---

## 8. Phased implementation plan

Each phase ends with something runnable. No phase may begin before the previous one's definition of
done is green in CI.

### Phase 0 — Foundations `[DONE]`

**Contents (already on disk):** `fxsound-core` (`lib.rs`, `messages.rs`, `settings.rs`);
`fxsound-dsp` (`biquad.rs`, `eq.rs`, `spectrum.rs`, `engine.rs`, `effects/{mod,fidelity,bass,surround,dynamic_boost}.rs`);
`fxsound-preset` (`lib.rs`, `store.rs`); `fxsound-ui` (`theme.rs`, `layout.rs`, `assets.rs`,
`state.rs`, `widgets/slider.rs`).
`effects/ambience.rs` is a documented pass-through stub and belongs to **Phase 2**.

**Definition of done:** `cargo test --workspace` green; `cargo clippy --workspace -- -D warnings`
clean; every constant in `layout.rs` and `theme.rs` has a `path:line` citation in its comment.
*(Met, except the ambience stub.)*

### Phase 1 — Silent audio pipeline

**Build:** the whole of `fxsound-audio`; `fxsound-app` reduced to a headless binary
(`fxsound --status`, `--power`, `--output`, `--preset`) with no window.

* T3: `MainLoopRc` + `ContextRc` + `CoreRc` on a spawned thread; registry walk → `SoundDevice`;
  `Metadata` proxy for `default.audio.sink` / `default.configured.audio.sink`.
* Two `StreamRc`s with the exact property sets from `docs/spec/12-audio-io.md:1283-1322` and the
  verified spellings in `docs/api/pipewire-0.10-rust.md:2215-2266`. `node.link-group = "fxsound"`
  on **both** — without it, the moment our sink becomes default, our own output stream autoconnects
  to our own sink and feeds back.
* `rules.rs` with unit tests covering all seven branches and the mono guard.
* Default-sink takeover and the five-step restore, wired to `Drop`, `SIGINT`, `SIGTERM`.
* Reconnect FSM with 200 / 400 / 800 / 1600 / 3200 / 5000 ms backoff.
* `Engine` present but `power = false` → clean pass-through.

**Definition of done:**
1. `wpctl status` shows `FxSound` as a sink; `pw-link -l` shows both nodes in one link group.
2. `mpv` into `fxsound_sink` is audible, bit-identical to direct playback (verified by capturing
   the target sink's monitor and diffing).
3. `kill -9` leaves **no orphan node** and audio returns within 1 s.
4. Clean quit and `SIGTERM` both restore `default.configured.audio.sink` to the pre-launch value,
   asserted with `pw-metadata -n default`.
5. `systemctl --user restart pipewire wireplumber` mid-playback → reconnect < 2 s, one audible gap,
   backoff never tighter than 200 ms.
6. USB DAC unplugged mid-playback → node 2 moves, node 1 untouched, clients never disconnected.
7. A mono-only sink yields `NoValidOutput`; mono + stereo yields `AskUserSelectOutput`.

### Phase 2 — DSP live

**Build:** `leveling.rs`; the real `ambience.rs`; `Engine` wired into `sink.process()`; the
parameter/meter/event plumbing of §4.1.

**Definition of done:**
1. Golden coefficient tests to ≥ 5 significant figures for `calc_parametric`, the bass biquad at
   90 Hz / Q 2.5, the Fidelity high-pass at 1745.4987 Hz, and the ten EQ band Qs
   (`docs/spec/09-dsp-eq.md:1144-1199`, `docs/spec/10-dsp-effects.md:1908-1967`).
2. End-to-end unit checks: with every slider at 0 and power on, a −20 dBFS sine comes out
   **−0.3 dBFS lower** (the maximizer's `max_output = 0.966051`), and never above it.
3. Ten minutes of playback under an allocator shim that aborts on any allocation on the data
   thread, with `stress-ng --cpu $(nproc)` running.
4. Effect knobs are audible and the 10-band spectrum tracks a swept sine correctly **at 48 kHz**
   (the re-derived analyser, not the 44.1 kHz-frozen original).
5. `Engine::reset` zeroes every item in the state table at `docs/spec/08-dsp-api.md:1132-1148`.

### Phase 3 — The Pro window

**Build:** `fxsound-ui::chrome`, `view::pro`, `view::controls`, `view::equalizer`,
`widgets::{visualizer,FxComboBox,FxPowerButton,FxImageButton,FxVerticalSlider,FxRotary,FxBalanceSlider}`;
`fxsound-app::{app,controller}`; the 100 ms tick and the 5-tick debounce.

**Definition of done:**
1. The window matches the ASCII wireframe at `docs/spec/01-window-layout.md:956-1012` — verified by
   screenshotting at `pixels_per_point = 1.0` and asserting each child rect to ±1 px.
2. Power off greys out the preset combo, controls, EQ and visualizer, and **not** the output combo
   (deliberate asymmetry, `docs/spec/01-window-layout.md:1386-1389`).
3. Moving an effect slider marks the preset modified (`*` suffix) and changes the sound within one
   quantum; a programmatic refresh does **not** mark it modified.
4. Preset switching autosaves the outgoing preset, prefers the autosave shadow on load, and the
   `*` survives a restart.
5. `RUST_LOG=debug` shows no repaint when the window is idle and no audio is flowing (0 % CPU at
   `ControlFlow::Wait`).

### Phase 4 — Lite view, tray, notifications, CLI

**Build:** `view::lite`; `tray.rs`; `notify.rs`; `ipc.rs`; `cli.rs` `apply_config`;
`status.rs`; `signals.rs`; the hamburger menu and `FxPresetNameEditor`.

**Definition of done:** the full parity checklist at `docs/spec/07-startup-tray.md:1544-1566`, plus:
`fxsound --status` prints the JSON on the **caller's** stdout with byte-identical field names, and
a second `fxsound --preset X` never starts a second process.

### Phase 5 — Settings, preset IO, i18n

**Build:** `view::settings` (3 panes, deferred viewport); `view::settings::outputs`;
`view::preset_io`; `dialogs.rs` (rfd portal, on a worker thread — **never** the blocking dialog on
T1); `i18n.rs`; `autostart.rs`; `widgets::{FxLanguageSwitcher,link,confirm_modal}`.

**Definition of done:** every settings key in `docs/spec/05-controller-model.md:496-531` round-trips
through `settings.toml`; the language switcher rebuilds the font atlas once per switch and never per
frame; "Reset presets to factory defaults" is behind a confirmation and routes deletions through
the XDG trash; export/import work through the portal inside and outside a sandbox.

### Phase 6 — Packaging and hardening

**Build:** icon theme (`hicolor` 16…256 + `scalable` + `symbolic` status icons regenerated from the
five-bar geometry at `docs/spec/07-startup-tray.md:144-155`, **transparent background**); install
rules for the two `.desktop` files and the systemd unit; `crash.rs` (`std::panic::set_hook` →
`$XDG_STATE_HOME/fxsound/`); xrun auto-step-up; optional `ashpd` GlobalShortcuts behind a feature;
optional zero-window `winit` `ApplicationHandler` (R-07).

**Definition of done:** a clean install on Hyprland, Sway, KDE Plasma and GNOME (with the
AppIndicator extension) reaches the tray, takes and restores the default sink, and survives a
compositor restart.

---

## 9. Deliberate deviations from the Windows build

Each is a documented bug in the original. Fixing them is a decision, recorded here so a future diff
against upstream does not "restore" them.

| # | Original behaviour | Cite | Port |
|---|---|---|---|
| D-1 | Lite error toast at `x = −50`, clipped | `docs/spec/01-window-layout.md:633-635` | Right-aligned to the output combo |
| D-2 | Horizontal slider fill uses `sliderPos` as a width → 8 px overshoot | `docs/spec/03-controls.md:205-209` | `slider_pos - rect.left()` |
| D-3 | Balance gradient ends at `x = 112` instead of 120 | `docs/spec/03-controls.md:210-214` | Runs to the track end |
| D-4 | Settings pane overhangs the content by 2 px; separator drawn 5 px off | `docs/spec/06-dialogs.md:221-224`, `:178-183` | One divider at the pane's left edge, pane width 447 |
| D-5 | Remove button 4 px off; selected-row underline drawn with a 0.5 px slope | `docs/spec/06-dialogs.md:373-374`, `:396-399` | Flat, correctly inset |
| D-6 | `isPowerOn()` returns true when bypassed | `docs/spec/08-dsp-api.md:1262-1266` | Never ported; `DspParams::power` is the truth |
| D-7 | `getEffectValue` returns 0..1 while `setEffectValue` takes 0..10; the cache starts stale | `docs/spec/08-dsp-api.md:1268-1275` | One scale per layer, conversions only in `fxsound_core::scale` |
| D-8 | Spectrum biquads frozen at 44.1 kHz → ~8.8 % error at 48 kHz | `docs/spec/04-equalizer-visualizer.md:1370-1377` | FFT analyser with rate-derived bin edges |
| D-9 | `fast_sqrt` bit-hack with 6 % error | `docs/spec/04-equalizer-visualizer.md:1072-1075` | `f32::sqrt` |
| D-10 | Zero-order-hold upsampling above 48 kHz, no anti-aliasing | `docs/spec/12-audio-io.md:730-736` | Run at the sink's native rate; PipeWire resamples if needed |
| D-11 | `Thread::sleep(200)` on the message thread after every output switch | `docs/spec/05-controller-model.md:1572` | `skip_ticks: u8` |
| D-12 | `Thread::sleep(2000)` to avoid losing a notification | `docs/spec/05-controller-model.md:1573` | Toast queue + `replaces_id` |
| D-13 | Update check fires only at exactly 10:00:00 | `docs/spec/05-controller-model.md:1574` | Auto-update deleted entirely |
| D-14 | Preset list in filesystem-glob order | `docs/spec/03-controls.md:1263-1267` | `PresetStore` sorts deterministically **[W]** |
| D-15 | Volume mirrored between the virtual and real endpoints | `docs/spec/12-audio-io.md:1250-1256` | Never write `Props` on the target sink; our node *is* the default control |
| D-16 | `user_selected_playback` read in four places, never written | `docs/spec/12-audio-io.md:1716-1721` | `set_output` writes it |
| D-17 | `savePreset("")` can shadow a factory preset into the user dir | `docs/spec/05-controller-model.md:706-708` | Invariant asserted inside the function |
| D-18 | N=31 EQ columns overlap by 8 px; "last child wins" | `docs/spec/04-equalizer-visualizer.md:1386-1390` | Interactive width clamped to `min(32, col_w)` |
| D-19 | Alt+drag "solo" collides with the compositor's window-move gesture | `docs/spec/04-equalizer-visualizer.md:1437-1439` | Rebound to Ctrl+Alt+drag, and surfaced with a per-band affordance |

Behaviours ported **verbatim** because they are the product's fingerprint: the 100 ms tick and the
5-tick (500 ms) processing debounce; the 600-tick (60 s) autosave; autosave-shadow semantics
(`modified` derived only from the shadow file's existence, `setPreset` prefers the shadow); the
`" *"` suffix; the single separator between factory and user presets; the 64-char preset name cap
with `<>:"/\|?*` stripped and case-insensitive uniqueness; the five quantisation rules
(master gain → integer, balance → integer, volume levelling → 0.5, filter Q → 0.5); wrap-around
preset/output cycling including the "skip < 2 channel devices" loop; device priority as array order
with an unlisted device sorting last; 7 s / 8 s notification timeouts, 3 lines maximum.

---

## 10. Risk register

| ID | Risk | Impact | Likelihood | Mitigation | Phase |
|---|---|---|---|---|---|
| **R-01** | **Hijacking `default.configured.audio.sink` fights WirePlumber's own policy, or a crash leaves the user silent.** This is the `CLAUDE.md` "affects system audio for all users" class. | Critical | Medium | Read + persist before the first write; restore on clean exit, `SIGINT`, `SIGTERM` and panic hook, **before** destroying the nodes; `priority.session = 500` so we never win implicitly; the whole of Phase 1 DoD tests 1–4 exist for this. | P1 |
| **R-02** | Two nodes land on different graph drivers → ring drift and periodic glitches. | High | Low | `node.want-driver = "true"` on node 1 so the real device drives both; instrument min/max ring fill over 60 s windows from day one; fall back to `SPA_IO_RateMatch` if drift is ever observed. | P1 |
| **R-03** | An allocation, lock or panic sneaks into `process()` and xruns the whole graph. | Critical | Medium | The §4.3 list; `let … else { return }` everywhere; `panic = "abort"`; the 10-minute allocator-shim test in the Phase 2 DoD; no `log::` anywhere under `fxsound-dsp` or the `process` closures. | P2 |
| **R-04** | **Gilroy is a commercial typeface** and is currently `include_bytes!`d by `fxsound-ui/src/theme.rs:221-223` into an AGPL binary. | High (legal) | High | Confirm redistribution rights. If absent, swap to Inter/Manrope/Figtree behind the same `fonts::{REGULAR,SEMIBOLD,BOLD}` names and re-measure every fixed-width label (the 40 px value labels, the 170 px hotkey caption, the `width − 37` combo clip, the 74 px EQ frequency column). Budget one layout pass. | P6, decide by P3 |
| **R-05** | JUCE `Font::withHeight(17.0)` ≠ `FontId::new(17.0, ..)`; every label is sized off 17/14/12 px. A 10 % error clips text in the 470×40 combos. | Medium | High | Calibrate once against a reference screenshot; if exact parity is needed, solve for `size` such that `fonts.row_height(&font_id) == 17.0`. Do it before Phase 3 geometry work is signed off. | P3 |
| **R-06** | `Ambience` is a stub. Shipping without it silently drops one of five headline effects. | High | Certain (today) | Phase 2 is not done until the Dattorro tank is in and its single 140 370-float ring is allocated in `new()`. | P2 |
| **R-07** | **eframe assumes a window exists for the process's lifetime**; hide-to-tray on Wayland cannot unmap a toplevel. | Medium | High | Phase 4 ships "minimise, tray restores". Phase 6 evaluates a hand-rolled `winit` 0.30 `ApplicationHandler` + `egui-winit`/`egui_glow` that can run with zero windows. Do not fight the framework in between. | P4 → P6 |
| **R-08** | GNOME has no SNI host; with `--hide` the app is invisible and unkillable from the UI. | Medium | High | Detect "no watcher within 5 s"; post a notification; refuse to start hidden when there is neither a watcher nor a notification daemon. | P4 |
| **R-09** | Alpha blending: JUCE composites in straight sRGB with no gamma correction; epaint blends in linear space. The many α 0.1 / 0.2 / 0.34 overlays will read lighter. | Medium | Medium | Compare the EQ fill and the panel backgrounds against a Windows screenshot early; adjust the alphas in one place (`Palette::color_alpha`) if needed, not at each call site. | P3 |
| **R-10** | `ksni` defaults to tokio while `notify-rust` defaults to async-io; both pull zbus with different feature sets. | Low | Medium | Enable `ksni/blocking` and keep both on a plain `std::thread` (T4). Never call `notify-rust`'s `.show()` from inside a ksni callback. If a clash appears, move ksni to `default-features = false, features = ["async-io","blocking"]`. | P4 |
| **R-11** | `pipewire`/`libspa` key constants are `#[cfg]`-gated; `NODE_LINK_GROUP`, `TARGET_OBJECT`, `NODE_WANT_DRIVER` do not exist without `v0_3_65`. | Low | Certain | Already handled in `crates/fxsound-audio/Cargo.toml`. Keep the comment. Literal strings are a valid fallback. | P1 |
| **R-12** | Preset compatibility: a `.fac` written by the port must reload in the Windows build. | Medium | Low | Round-trip tests against all 32 shipped presets, byte-for-byte, including the `%g` float formatting, the `Main 2` hole, the seven app-dependent integers and the 1-based band numbering. `fxsound-preset` already implements this — keep the tests. | P0 (done), re-run each phase |
| **R-13** | PipeWire version floor: `target.object` needs ≥ 0.3.64, `node.link-group` settled ~0.3.43. | Low | Low | Declare a hard minimum of PipeWire 0.3.65 / WirePlumber 0.4.14; detect via `pw_get_library_version()` at startup and refuse with a clear message rather than half-working. | P1 |
| **R-14** | Translations are not in this checkout (`Resources/Strings/` is empty); only `BinaryData` symbol names survive. | Medium | Certain | Ship English only in Phases 1–5. Phase 5 builds the catalogue machinery with a validating format (named placeholders, not `%s`) so a bad translation can never be a format-string bug. Extraction from `BinaryData.cpp` is a separate work item. | P5 |
| **R-15** | Channel maps: PipeWire delivers an explicit `audio.position`; the DSP assumes Windows WAVE order (FL FR FC LFE BL BR [SL SR]). | Medium | Medium | Declare `audio.position` on node 1 to mirror the target and let PipeWire's mixer remix; inside `process()` only the identity case exists. Never assume WAVE order — an LFE through the widener is an audible bug. | P2 |
| **R-16** | RTKit may refuse RT priority (no D-Bus, no portal, no `CAP_SYS_NICE`) and `process()` runs `SCHED_OTHER`. | Medium | Medium | Verify after the stream reaches `Streaming` by reading `sched_getscheduler` on the data thread; log once and raise the ring target fill instead of failing. **Never call `sched_setscheduler` ourselves.** | P1 |
| **R-17** | Accessibility: eframe's `accesskit` feature is **off** at our feature set, so there is no AT-SPI tree at all, and every control is custom-painted. | Low | Certain | Accept the downgrade for v1 and say so in the README. Revisit by enabling `accesskit` and adding `widget_info` to each custom widget. | P6+ |

---

## Open questions / risks for the Rust port

1. **Should the Lite view keep existing at all?** Its entire reason for being on Windows is the
   tray-corner snap, which is gone (§7.6). Without it, Lite is just a smaller window with two combo
   boxes. Recommendation: keep it — it is two combo boxes of code and it is what the flip button
   means — but do not spend effort on placement. Confirm with the product owner.
2. **`node.hidden` on node 2.** It is the natural analogue of `AUDCLNT_SESSIONFLAGS_DISPLAY_HIDE`,
   but some WirePlumber versions skip policy linking for hidden nodes entirely, which would break
   us. Ship with it **unset**, rely on `node.link-group`, and only revisit after testing against
   pavucontrol, `wpctl status`, GNOME Settings and Plasma's applet.
3. **`priority.session`.** `1010` (above typical ALSA sinks) means we may be auto-selected the first
   time we run on a machine where the user never picked a default. This document specifies **500**
   (never wins implicitly) plus the explicit §7 takeover path — but that is a product decision, and
   it differs from the Windows behaviour.
4. **Mono outputs are refused, not downmixed.** Ported faithfully from
   `SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES`. There is no driver bug on Linux forcing this, and a
   mono BT headset is perfectly drivable. Fixing it would delete the `NoValidOutput` /
   `AskUserSelectOutput` states entirely. Decision deferred to Phase 1 review.
5. **Does the maximizer's 16-bit quantise/shaped-dither stage actually run?** `Play32.c:414-415`
   sets it but `MAXIMIZE_QUANTIZE_ON` is never written by `dfxpComm.cpp`. Adding a spurious 16-bit
   dither to a float pipeline would raise the noise floor by ~90 dB. Verify against a real build
   before Phase 2 signs off; the current port omits it.
6. **What is the Linux trigger for "fatal, quit"?** The Windows build quits hard when the virtual
   device is missing or `AudioPassthru::init()` fails. On Linux those failure modes (PipeWire
   restarting, a sink hot-unplugging) are *recoverable* and must not quit the app. This document
   specifies: never quit on an audio error; surface it as `AudioError` and keep retrying. Confirm
   that is acceptable product behaviour.
7. **Settings-dialog modality.** The C++ blocks in `runModalLoop()` and runs
   `refreshOutputList()` *after* it returns. In the deferred-viewport design there is no "after";
   the refresh must happen on settings-close. Verify no behaviour depends on the modal block.
8. **Version string.** `ProjectInfo::versionString` says `1.2.14.0` while the `.jucer`, the `.rc`,
   the vcxproj and the installer all say `1.2.15.0`. The port uses `env!("CARGO_PKG_VERSION")`
   (`0.1.0`) for `status.json`, the log banner and the Help pane — a clean break. Confirm that
   nothing downstream (a future `fxmcp` port) pins the old string.
9. **Language codes `ua` and `ba`** are wrong per ISO 639-1 (`uk`, `bs`). Fixing them needs a
   migration for the persisted `language` value. Decide in Phase 5; the schema key is already in
   `fxsound-core::settings`.
10. **RTL is a new feature, not a port.** JUCE 6.1.6 does no BiDi and no Arabic shaping; epaint does
    none either. Shipping `ar`/`fa` means adopting `cosmic-text`/`harfbuzz_rs` + `unicode-bidi` +
    a mirrored-layout pass through every custom widget. If that is out of budget, ship those two
    locales **disabled** rather than visibly broken.
11. **`packaging/hyprland.conf.example` contradicts `layout.rs`** on the window sizes (`1040x511` /
    `570x162` vs the contract `1040×588` / `550×189`) and uses hotkey letters (F/D/P/O) that differ
    from the Windows defaults (A/Z/Q/E). Both are fine choices, but they must be reconciled with the
    settings pane's read-only hotkey display in Phase 5 so the app does not show one thing and the
    compositor do another.
12. **Where does the ambience tank's 140 370 floats come from at 192 kHz?** `DSPS_SOFT_MEM_LEX_LENGTH`
    is derived from a 96 kHz assumption (`c_dsps.h:92`). Running at 192 kHz native (D-10) means
    either scaling the tank — changing the reverb's character — or capping the DSP rate at 96 kHz and
    letting PipeWire resample. Resolve before Phase 2 implements `Ambience::set_sample_rate`.
