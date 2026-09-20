# FxSound for Linux

A native Linux port of [FxSound](https://www.fxsound.com) — the system-wide audio enhancer — written
in Rust with [egui](https://github.com/emilk/egui), rendering natively on Wayland and processing
audio through PipeWire.

This is a **fork**, not a wrapper: the Windows application is C++/JUCE talking to a proprietary
virtual audio driver through WASAPI and COM, none of which exists here. Every layer has been
re-implemented, but the DSP is ported from the original C rather than reinvented, so presets voiced
on Windows sound the same here.

> Status: it builds, runs and passes 767 tests, and the audio path has been verified end to end
> against a live PipeWire session — the virtual sink appears and becomes the session default,
> applications play through it, processed audio reaches the chosen output device, and picking a
> microphone instead turns FxSound into a virtual source running [a voice chain of its
> own](#the-microphone-chain). The interface speaks the desktop's language, using the Windows
> build's own translation tables. See [Implementation status](#implementation-status).

---

## Why a rewrite rather than a port of the existing code

| Windows FxSound | Why it cannot come across | What replaces it |
|---|---|---|
| JUCE 6.1.6 GUI | AGPL framework built around its own event loop, `LookAndFeel` painting and HWND-backed windows | egui 0.36 with custom painters, one per JUCE `LookAndFeel` override |
| FxSound Audio Enhancer virtual driver | A signed Windows kernel driver, not in this source tree and not buildable for Linux | A PipeWire virtual sink the application owns, plus a capture/render pair |
| WASAPI + COM endpoint enumeration | Windows-only API surface | The PipeWire registry |
| `RegisterHotKey` global shortcuts | A Wayland client cannot grab global shortcuts, by design | Compositor keybinds that call the CLI, forwarded over a control socket |
| Registry persistence | — | TOML under `$XDG_CONFIG_HOME`, with the original's key names kept |

The DSP is the exception. `dsp/` in the upstream tree is real C source, and it is ported
line-by-line: the filter designs, the effect algorithms and their coefficient mappings are
reproduced exactly, including a handful of quirks that a "cleaner" implementation would break.

## Requirements

- Rust 1.98.1 (edition 2024)
- PipeWire 1.0 or newer
- A Wayland compositor. Verified on a 17-check acceptance run across **sway, labwc, river, wayfire,
  KWin, niri and cosmic-comp**, and developed against Hyprland 0.56. X11 works through XWayland but
  is not a target.
- Build-time: `libxkbcommon`, `wayland`, `vulkan` or `libGL`, `pipewire` development headers, and
  `clang` — `pipewire-sys` runs bindgen in its build script and fails without libclang.

```bash
cargo build --release --bin fxsound
```

The binary lands at `target/release/fxsound` and can be run straight from there.

## Installing

Build a real package rather than copying the binary around: it puts the presets, the `.desktop`
entry, the icons and the systemd user unit where the desktop expects them, and your package manager
takes all of it back out again.

### Arch and derivatives

From the working tree you already have:

```bash
cd packaging
makepkg -f
sudo pacman -U fxsound-linux-0.3.0-1-x86_64.pkg.tar.zst
```

`makepkg` runs the whole test suite as part of the build; pass `--nocheck` to skip it.

From the AUR, two packages: **`fxsound-linux-bin`** installs the prebuilt release tarball and needs
no Rust toolchain, and **`fxsound-linux-git`** builds the current `main` from source.

### Debian and Ubuntu

`packaging/debian/` is a full Debian source package; `dpkg-buildpackage` wants it at the root of the
tree, so copy it there first:

```bash
cp -a packaging/debian debian
dpkg-buildpackage -us -uc -b
sudo apt install ../fxsound-linux_0.3.0-1_amd64.deb
```

The `.deb` lands beside the source tree, not inside it. Use `apt` rather than `dpkg -i` so the
runtime dependencies are pulled in with it, and `-b` so only the binary package is built.
`DEB_BUILD_OPTIONS=nocheck` skips the test suite, which `debian/rules` otherwise runs in full.

One thing will bite before anything else: `Cargo.toml` sets `rust-version = "1.98.1"` and cargo
enforces that against any rustc, so neither Debian 13 (rustc 1.85) nor Ubuntu 24.04 (1.75) can build
this from the archive's toolchain. Install one from rustup, or build in a container. The reasoning,
and why lowering the floor is not the fix, is at the top of
[`packaging/debian/control`](packaging/debian/control).

### Fedora

```bash
rpmbuild -ba packaging/fedora/fxsound.spec
sudo dnf install ~/rpmbuild/RPMS/x86_64/fxsound-linux-0.3.0-1.*.x86_64.rpm
```

The spec needs a vendored-dependency tarball beside it;
[`packaging/fedora/README.md`](packaging/fedora/README.md) says how to make one and how to build
in mock.

### Any distribution: the tarball

For anything else, or for anyone who would rather no package manager were involved:

```bash
packaging/build-tarball.sh
tar xf dist/fxsound-0.3.0-x86_64.tar.gz
sudo ./fxsound-0.3.0-x86_64/install.sh        # /usr/local unless you name another prefix
```

`/usr/local` is searched for presets alongside `/usr`, so nothing needs configuring afterwards.

A prebuilt tarball is attached to each [release](https://github.com/blackixxce12/fxsound-linux/releases);
check it against its `.sha256` before unpacking. It unpacks to the same layout but is assembled by
CI rather than by the script above, so it carries no `install.sh` — copy `bin/`, `lib/` and `share/`
into your prefix by hand. That binary is linked against the glibc of the runner that built it, so on
an older distribution build from source instead.

### What lands where

| Path | What |
|---|---|
| `/usr/bin/fxsound` | the binary |
| `/usr/share/fxsound/presets/Factsoft/`, `BonusPresets/` | the 34 factory and bonus `.fac` presets |
| `/usr/share/fxsound/presets/Input/` | the 10 voice presets, in TOML |
| `/usr/share/applications/com.fxsound.FxSound.desktop` | the launcher entry |
| `/usr/lib/systemd/user/fxsound.service` | `systemctl --user enable --now fxsound` |
| `/usr/share/icons/hicolor/*/apps/fxsound.png` | the icon |
| `/usr/share/doc/fxsound-linux/` | the Hyprland rules and the autostart entry |

There is no AppImage or Flatpak. A Flatpak would be actively counterproductive here: the whole
point of the app is to own a node in your PipeWire graph and drive your real output device, and the
sandbox exists to prevent exactly that.

## Running under Hyprland

The window is frameless and has a fixed design size, so it wants to float rather than tile. Copy the
rules from [`packaging/hyprland.conf.example`](packaging/hyprland.conf.example) into your
`hyprland.conf`:

```conf
windowrulev2 = float,    class:^(com\.fxsound\.FxSound)$
windowrulev2 = noborder, class:^(com\.fxsound\.FxSound)$
```

Closing the window never quits. The ✕ button, the minimise button and the compositor's own close
request (`killactive`, usually bound to `Super+Q`) all hide the window while the audio keeps
processing; the app lives on in the system tray, and the tray's **Open** or `fxsound --show` bring
the window back. **Exit** in the tray menu or `fxsound --quit` are the only ways out. This is what
the Windows build does too — with the difference that a Wayland client cannot unmap its own
toplevel, so "hide" here really destroys the window and recreates it on demand.

Fullscreen (Hyprland's `fullscreen` dispatcher, Super+F in many configurations) and maximised
windows scale the fixed design up to fit the monitor and centre it; the app never draws into a
corner of an oversized surface.

Global shortcuts are the one feature that cannot work the way it does on Windows. A Wayland client
is not allowed to grab keys it does not have focus for — that is a deliberate security property of
the protocol, not a gap. The same file binds them in the compositor instead, which reaches the
running instance through its control socket:

```conf
bind = CTRL SHIFT, F, exec, fxsound --next-preset
bind = CTRL SHIFT, P, exec, fxsound --toggle-power
```

## How the audio path works

```
   your apps
       │  play into
       ▼
┌──────────────────┐     ┌─────────────────────┐     ┌──────────────────┐
│  FxSound sink    │────►│  DSP engine         │────►│  your real sink  │
│  (virtual, ours) │     │  EQ → effects → lim │     │  (you choose it) │
└──────────────────┘     └─────────────────────┘     └──────────────────┘
```

FxSound publishes its own sink. Everything written there is processed and rendered to whichever real
device you pick in the app. That is the same shape as the Windows virtual driver, implemented with
PipeWire nodes instead of a kernel driver — which also means uninstalling is `pkill fxsound` and
your audio comes straight back.

Pick a microphone instead and the whole thing reverses: the sink pair comes down and a capture
stream feeds a virtual source, `FxSound (Input)`, that applications record from. One direction at a
time, which is what keeps the routing unambiguous.

**It does not take over your default output on its own.** The sink is published with
`priority.session = 500` so it never wins implicitly, and the default is only claimed when you ask
for it. When you do, the previous default is remembered first and handed back on the way out —
before the nodes are destroyed, so there is never a moment where the default names a node that no
longer exists. This is the one part of the port that can affect audio for the whole session, and it
is written to fail safe.

## The signal chain

For an output device, processing order is fixed by the original and is **not** the order the
sliders appear in:

```
in ─► 31-band graphic EQ ─► master gain · balance ─► volume levelling
   ─► Fidelity ─► Ambience ─► Surround ─► Bass ─► Dynamic Boost ─► out
```

| Effect | Algorithm |
|---|---|
| Fidelity | 2nd-order Butterworth high-pass at 1745.5 Hz into a sine waveshaper that deliberately folds over |
| Ambience | Dattorro/Lexicon-224-style figure-of-eight plate reverb |
| Surround | Mid/side gain widener, side ×(1+3i), mid ×(1−0.3i) |
| Bass | One parametric peaking biquad, 90 Hz, Q 2.5, 0–15 dB |
| Dynamic Boost | ~1.6 s RMS auto-gain plus a 0.75 ms look-ahead brick-wall limiter |

Total added latency is the limiter's look-ahead: 0.75 ms.

Every stage follows the channel layout the device reports rather than assuming the first two
channels are the front pair.

## The microphone chain

A microphone does not get the music chain pointed at a different input. Music processing exists to
make music sound *bigger* — reverberation, a bass lift, a stereo widener — and every one of those is
the opposite of what a voice wants. So a microphone gets a chain of its own:

```
mic ─► RNNoise ─► high-pass ─► gate ─► 10-band EQ ─► de-esser ─► compressor ─► makeup ─► limiter ─► out
```

| Stage | What it does |
|---|---|
| RNNoise | Recurrent-network denoiser. Off unless the preset asks. ~44 dB off a desk microphone's hum-under-hiss; on undifferentiated white noise, ~1 dB — it separates speech from noise, and white noise gives it nothing to separate |
| High-pass | Butterworth, 2nd or 4th order. Desk rumble and plosives sit ten to twenty dB above the voice below 150 Hz; left in, they hold the gate open through every pause |
| Gate | Downward expander with a threshold, a ratio, a floor, hold, and peak or RMS detection — it turns the room down rather than switching it off |
| Equalizer | Ten bands, the same graphic EQ the output chain uses |
| De-esser | Split-band, fourth-order Linkwitz–Riley crossover, acting only on the high band |
| Compressor | Threshold, ratio, soft knee, attack, release, peak or RMS detection |
| Makeup | Applied after everything that measures, so a preset's thresholds mean what they say |
| Limiter | 1 ms look-ahead, always running. It is the only stage that cannot be switched off: makeup gain is the one control here that can push a sample past full scale |

The order is not a preference — each position is argued, with its reason, at the top of
[`crates/fxsound-dsp/src/input/chain.rs`](crates/fxsound-dsp/src/input/chain.rs). Denoising goes
first because everything below it measures a level. The gate goes before the equalizer so that what
it measures is the microphone and not the preset's own presence lift. The de-esser goes before the
compressor, because a compressor in front would ride the sibilant and duck the word behind it.

Added latency is the limiter's 1 ms, plus RNNoise's 10 ms when it is on. Both are published to
PipeWire and kept current, so a recording application is never told a figure that has since changed.
The capture stream asks for 48 kHz whatever the microphone runs at — RNNoise exists at 48 kHz and
nowhere else — so a voice preset means one thing on every device.

### Voice presets

Ten ship, in TOML rather than `.fac`: a `.fac` is a byte-for-byte contract with the Windows build
and has nowhere to put a gate threshold. A stage that is switched off has no table in the file, so
there is no way to ship a full set of numbers that nothing reads.

| Preset | For |
|---|---|
| Clean Voice | A desk microphone, tidied rather than styled — Discord, Teams, Zoom or in-game. The one to compare the others against |
| Flat | Protection and nothing else: the high-pass and the ceiling, no voicing at all |
| Headset | A boom microphone two to five centimetres from the mouth: proximity and plosives held down |
| Laptop Mic | A built-in microphone half a metre away, with a fan running — the repair preset |
| Broadcaster | The radio voice: chest, presence, and a compressor that rides phrases rather than syllables |
| Streaming | A live send to Twitch, YouTube or Kick: dense and bright, at the level a phone speaker needs |
| Podcast | Recorded speech on its way to an editor: gentle, warm, headroom left on purpose |
| Studio | Recording into a DAW: one gentle compressor and nothing else |
| Bright Voice | For a dull or chesty voice: presence and air lifted, the low end eased |
| Warm Voice | For a thin or distant-sounding microphone: chest added, presence eased back |

The picker shows the voice set in front of a microphone and the `.fac` set in front of a speaker, and
never mixes them. Your choice is remembered **per direction**, so a preset picked for a microphone
does not follow you back to your speakers. The reasoning behind every number in these files is in
`docs/input-presets-decision.md`.

While a microphone is selected the window says so rather than pretending: the five effect sliders are
drawn disabled with the reason underneath, the chain's stages read out along the bottom of the panel,
and an equalizer band the device's sample rate cannot carry is struck through instead of left looking
live.

## Presets

`.fac` files are read and written in the original's format — a line-based text format, despite the
extension. Every preset that ships with FxSound round-trips byte-for-byte, so presets copy in both
directions between this port and the Windows build.

| | Path |
|---|---|
| Factory presets | `/usr/share/fxsound/presets/Factsoft/`, `BonusPresets/` |
| Voice presets | `/usr/share/fxsound/presets/Input/` |
| Your presets | `~/.local/share/fxsound/presets/` |
| Unsaved edits | `~/.local/share/fxsound/presets/AutoSave/` |
| Settings | `~/.config/fxsound/settings.toml` |

The settings file keeps the original's key names so it can be diffed against the Windows
`FxSound.settings`. Settings and presets are written durably — temporary file, fsync, rename — so an
interrupted save cannot truncate what was there.

## Deliberate differences from the Windows build

Each of these is a considered decision, not an oversight:

- **No update check and no telemetry.** The `automatic_updates` toggle exists so the Settings window
  matches, but nothing contacts the network. Updates come from your package manager.
- **Deterministic preset ordering.** The original lists presets in filesystem-glob order, which is
  arbitrary. This port sorts factory presets numerically and the rest by name.
- **Frame-rate-independent visualizer decay.** The original decays its bars once per timer tick;
  this port decays by elapsed time, so the animation looks the same at 60 and 165 Hz.
- **Q multiplier no longer resets the band layout.** Changing the filter width in the original
  silently discards preset-supplied band frequencies and resets the sample rate to 44100 until the
  next buffer. That is a bug; this port only redesigns the coefficients.
- **Global hotkeys live in the compositor.** See above.
- **Window position is not restored.** Wayland gives a client no way to place its own toplevel. The
  setting is still written so it survives a move back to X11.
- **No Donate button, no update check, no bonus-preset download, no Help center.** The heart in
  the title bar and the Donate items in the menu and the tray are gone: this fork is not the
  upstream developers' product and must not solicit money for them. "Check for updates" and the
  "Automatic updates" toggle need a network the package never uses, the bonus presets ship inside
  the package, and Settings ▸ Help keeps only the version and a **Changelog** that opens the
  bundled `CHANGELOG.md` in-app instead of the upstream website. "Always On Top" is also absent:
  winit ignores window levels on Wayland, and a control that does nothing is worse than none.
- **Translated, from the original's own tables.** The 28 JUCE `LocalisedStrings` files embedded in
  the Windows binary (`assets/translations/`, extracted from its `BinaryData.cpp`; Hungarian was
  declared but never shipped) are embedded here and looked up by the same English keys the C++
  passes to `TRANS`. The language follows the desktop session (`LC_ALL`/`LC_MESSAGES`/`LANG`)
  unless one is picked in Settings ▸ General or with `--language <code>` (`--language system`
  returns to following the desktop). Strings this port added are in
  `assets/translations/port/`. Right-to-left scripts render left-to-right — egui has no bidi.
- **Desktop notifications** for preset, output and power changes, the way the original's tray
  balloons announce them, through `org.freedesktop.Notifications`; *Hide notifications* in Settings
  silences them.
- **FxSound takes the session default automatically — politely.** Picking an output device makes
  `FxSound (Output)` the default sink so every application plays through it without any manual
  routing; the previous default is remembered first and handed back on exit or when the direction
  changes. Only `default.configured.audio.*` is ever written; `default.audio.*` stays WirePlumber's.
- **Input mode is a different processor, not the same one pointed elsewhere.** The device list is
  split into *Output* and *Input* sections. Choosing a microphone tears the sink pair down, builds a
  capture stream feeding a virtual source, `FxSound (Input)`, which becomes the default microphone,
  and runs [the voice chain](#the-microphone-chain) rather than applying reverberation and a bass
  lift to someone's speech. Choosing an output again reverses it.
- **System-visible names follow the system locale.** The node descriptions shown by pavucontrol and
  desktop volume widgets are `FxSound (Вывод)` / `FxSound (Ввод)` on a Russian desktop,
  `FxSound (Ausgabe)` / `FxSound (Eingabe)` on a German one, and so on for the thirteen languages
  the original ships; English otherwise. Node *names* (`fxsound_sink`, `fxsound_source`, …) are
  fixed ASCII, because they are what gets written into metadata.

The two known painting artefacts in the original — the slider fill overshooting its track by 8 px,
and the balance gradient ending 8 px early — *are* reproduced, because matching the look is the
point. `widgets::slider::Fidelity::Corrected` turns the first one off.

## Implementation status

| Component | Tests | State |
|---|---:|---|
| `fxsound-core` — shared types, scales, settings, translations | 36 | complete |
| `fxsound-preset` — `.fac` reader/writer, TOML voice presets, preset store | 22 | complete |
| `fxsound-dsp` — biquads, graphic EQ, five effects, limiter, leveller, spectrum, engine, voice chain | 251 | complete |
| `fxsound-ui` — palettes, geometry, assets, widgets, views, dialogs | 281 | complete |
| `fxsound-audio` — PipeWire backend, output and input modes | 57 | complete, verified on a live PipeWire 1.6 session |
| `fxsound-app` — controller, CLI, IPC, tray, notifications, window shell | 120 | complete |

Verified end to end: the window opens on Wayland at the original's exact geometry, a preset loads
from disk and drives both the sliders and the equalizer curve, the Settings pane opens, the CLI
parses every original option and reaches a running instance over the control socket, audio rendered
offline through `cargo run -p fxsound-dsp --example process_wav` measures the preset's equalizer
curve back out of the rendered file, and on a live PipeWire session the sink is published as
**FxSound (Output)**, claims the session default, accepts a stream, and hands processed samples to
the chosen output device. Verified on Hyprland 0.56 as well: the window survives on a hidden
workspace without tripping the compositor's "not responding" watchdog (the control socket keeps
answering while the surface is unmapped), ✕ / minimise / `killactive` hide to the tray and
`fxsound --show` brings a fresh, fully painted window back, the hamburger menu opens with the
original's items, and switching to the microphone and back moves the session default source to
**FxSound (Input)** and restores it afterwards.

Beyond Hyprland, a 17-check acceptance run — window, tray, control socket, node publication,
default claim and hand-back — passes on **sway, labwc, river, wayfire, KWin, niri and cosmic-comp**,
each in a disposable session with its own PipeWire graph, so the checks never touch the host's audio.

Measuring a running instance from outside is harder than it looks: recording the output device's
monitor picks up **everything** playing on that device, so anything else the machine is playing
buries the test signal. Point FxSound at an idle device, or instrument the ring between its two
nodes, rather than trusting a monitor capture.

### Known limitation: no second window

`eframe` 0.36's glow multi-viewport path creates a child toplevel and its GL surface on Wayland and
then never composites anything into it — confirmed here on Hyprland 0.56 with the NVIDIA driver,
where the Settings window appeared in `hyprctl clients` at the right size while even a solid fill
over its whole area stayed invisible. Settings is therefore drawn inside the main window over a
dimmed backdrop, and the window grows to fit it. Worth re-testing against the wgpu backend and
against a Mesa driver before calling it an upstream bug.

## Documentation

`docs/spec/` holds the reverse-engineering specification this port was written against — roughly
20,000 lines covering every subsystem of the original, with citations down to the source line. It is
worth reading before changing any DSP code. `docs/api/` holds verified API cheatsheets for
egui/eframe 0.36 and pipewire-rs 0.10, quoted from the vendored crate sources. The voice chain has
its own reasoning in `docs/voice-presets-research.md`, `docs/input-presets-decision.md` and
`docs/preset-evaluation.md`. [`CHANGELOG.md`](CHANGELOG.md) is what changed and when.

## Licence

AGPL-3.0-or-later, inherited from upstream FxSound.

The bundled artwork and the Gilroy typeface come from the upstream repository and carry their own
terms; the Noto fallback faces are SIL OFL. A redistributable package may need to replace Gilroy —
it is a commercial font and its presence in the upstream tree is not itself a licence to
redistribute.

## Credits

FxSound is by [FxSound LLC](https://www.fxsound.com), with major DSP contributions from
[Theremino](https://www.theremino.com). This port would not be possible without their decision to
open the source.
