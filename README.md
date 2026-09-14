# FxSound for Linux

A native Linux port of [FxSound](https://www.fxsound.com) — the system-wide audio enhancer — written
in Rust with [egui](https://github.com/emilk/egui), rendering natively on Wayland and processing
audio through PipeWire.

This is a **fork**, not a wrapper: the Windows application is C++/JUCE talking to a proprietary
virtual audio driver through WASAPI and COM, none of which exists here. Every layer has been
re-implemented, but the DSP is ported from the original C rather than reinvented, so presets voiced
on Windows sound the same here.

> Status: it builds, runs and passes 630 tests, and the audio path has been verified end to end
> against a live PipeWire session — the virtual sink appears and becomes the session default,
> applications play through it, processed audio reaches the chosen output device, and picking a
> microphone instead turns FxSound into a virtual source. The interface speaks the desktop's
> language, using the Windows build's own translation tables. See
> [Implementation status](#implementation-status).

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
- A Wayland compositor; developed against Hyprland 0.56. X11 works through XWayland but is not a
  target.
- Build-time: `libxkbcommon`, `wayland`, `vulkan` or `libGL`, `pipewire` development headers

```bash
cargo build --release
```

The binary lands at `target/release/fxsound` and can be run straight from there.

## Installing

On Arch and its derivatives, build a real package rather than copying the binary around — it puts
the factory presets, the `.desktop` entry and the icons where the desktop expects them, and
`pacman -R` takes all of it back out again:

```bash
cd packaging && makepkg -f
```

That produces `fxsound-linux-0.1.0-1-x86_64.pkg.tar.zst` next to the `PKGBUILD`. Install it with:

```bash
sudo pacman -U packaging/fxsound-linux-0.1.0-1-x86_64.pkg.tar.zst
```

`makepkg` runs the whole test suite as part of the build; pass `--nocheck` to skip it. The package
installs:

| Path | What |
|---|---|
| `/usr/bin/fxsound` | the binary |
| `/usr/share/fxsound/presets/` | the 31 factory and bonus presets |
| `/usr/share/applications/com.fxsound.FxSound.desktop` | the launcher entry |
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

**It does not take over your default output on its own.** The sink is published with
`priority.session = 500` so it never wins implicitly, and the default is only claimed when you ask
for it. When you do, the previous default is remembered first and handed back on the way out —
before the nodes are destroyed, so there is never a moment where the default names a node that no
longer exists. This is the one part of the port that can affect audio for the whole session, and it
is written to fail safe.

## The signal chain

Processing order is fixed by the original and is **not** the order the sliders appear in:

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

## Presets

`.fac` files are read and written in the original's format — a line-based text format, despite the
extension. Every preset that ships with FxSound round-trips byte-for-byte, so presets copy in both
directions between this port and the Windows build.

| | Path |
|---|---|
| Factory presets | `/usr/share/fxsound/presets/` |
| Your presets | `~/.local/share/fxsound/presets/` |
| Unsaved edits | `~/.local/share/fxsound/presets/AutoSave/` |
| Settings | `~/.config/fxsound/settings.toml` |

The settings file keeps the original's key names so it can be diffed against the Windows
`FxSound.settings`.

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
- **Input mode.** The device list is split into *Output* and *Input* sections. Choosing a
  microphone tears the sink pair down and builds a capture stream feeding a virtual source,
  `FxSound (Input)`, which becomes the default microphone; choosing an output again reverses it.
  One direction at a time, which is what keeps the routing unambiguous.
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
| `fxsound-core` — shared types, scales, settings, translations | 20 | complete |
| `fxsound-preset` — `.fac` reader/writer, preset store | 14 | complete |
| `fxsound-dsp` — biquads, graphic EQ, five effects, limiter, leveller, spectrum, engine | 155 | complete |
| `fxsound-ui` — palettes, geometry, assets, widgets, views, dialogs | 275 | complete |
| `fxsound-audio` — PipeWire backend, output and input modes | 56 | complete, verified on a live PipeWire 1.6 session |
| `fxsound-app` — controller, CLI, IPC, tray, notifications, window shell | 110 | complete |

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
egui/eframe 0.36 and pipewire-rs 0.10, quoted from the vendored crate sources.

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
