# FxSound for Linux

A native Linux port of [FxSound](https://www.fxsound.com) — the system-wide audio enhancer — written
in Rust with [egui](https://github.com/emilk/egui), rendering natively on Wayland and processing
audio through PipeWire.

This is a **fork**, not a wrapper: the Windows application is C++/JUCE talking to a proprietary
virtual audio driver through WASAPI and COM, none of which exists here. Every layer has been
re-implemented, but the DSP is ported from the original C rather than reinvented. Since 0.4.0 it
also fixes defects that came across with it, so a preset voiced on Windows sounds close to, but not
exactly, the same here; [the deliberate differences](#deliberate-differences-from-the-windows-build)
say what changed. A mode that restores the original behaviour, «Like FxSound for Windows» in
Settings ▸ Experimental (`--windows-parity=off|interface|sound`), is being built for 0.5.0: the
setting is there, `sound` already plays Volume Leveling, Dynamic Boost, Ambience, the equalizer,
the master gain and the balance as the Windows build does and reads a preset as it does, moving
to it and back while something plays does not click, and the rest of what each level changes
arrives over the release
([`docs/0.5.0-windows-parity.md`](docs/0.5.0-windows-parity.md)). Its fourth level, Everything,
comes in a later version; 0.5.0 refuses `full`, and runs a `settings.toml` that says `full` as
`sound` while keeping `full` in the file for that version.

It processes what the machine plays and, at the same time, a microphone: each has a lane of its
own, with its own device and preset, and an application can have a preset of its own on either.
A running instance answers on the command line, over D-Bus and to compositor keybindings, and
streams its changes to a status bar.

FxSound for Linux is an independent community project. It is not affiliated with, endorsed by or
supported by FxSound LLC; please report problems with this port here, not to them.

> Status: it builds, runs and passes 2,930 tests. The audio path has been verified end to end
> against a live PipeWire session — the virtual sink appears and becomes the session default,
> applications play through it, processed audio reaches the chosen output device, and a microphone
> feeds a virtual source through [a voice chain of its own](#the-microphone-chain). Both lanes at
> once, the per-application routes and echo cancellation are tested against a private PipeWire
> daemon. The interface speaks the desktop's language, using the Windows build's own translation
> tables. See [Implementation status](#implementation-status).

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
line-by-line: the filter designs, the effect algorithms and their coefficient mappings come from
the original, and where the original is defective 0.4.0 fixes it rather than copying it (see
[the deliberate differences](#deliberate-differences-from-the-windows-build)).

## Requirements

- Rust 1.98.1 (edition 2024)
- PipeWire 1.0 or newer, and WirePlumber (or another session manager) to move the session default
  and applications' streams. Echo cancellation needs PipeWire's WebRTC canceller,
  `aec/libspa-aec-webrtc`; without it the microphone runs as before and says why.
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
sudo pacman -U fxsound-linux-0.4.0-1-x86_64.pkg.tar.zst
```

`makepkg` runs the whole test suite as part of the build; pass `--nocheck` to skip it.

For the AUR there is one recipe, **`fxsound-linux-bin`** (`packaging/aur/fxsound-linux-bin/`): it
installs the prebuilt release tarball and needs no Rust toolchain. To build from source, use
`packaging/PKGBUILD` above.

### Debian and Ubuntu

`packaging/debian/` is a full Debian source package; `dpkg-buildpackage` wants it at the root of the
tree, so copy it there first:

```bash
cp -a packaging/debian debian
dpkg-buildpackage -us -uc -b
sudo apt install ../fxsound-linux_0.4.0-1_amd64.deb
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
sudo dnf install ~/rpmbuild/RPMS/x86_64/fxsound-linux-0.4.0-1.*.x86_64.rpm
```

The spec needs a vendored-dependency tarball beside it;
[`packaging/fedora/README.md`](packaging/fedora/README.md) says how to make one and how to build
in mock.

### Any distribution: the tarball

For anything else, or for anyone who would rather no package manager were involved:

```bash
packaging/build-tarball.sh
tar xf dist/fxsound-linux-0.4.0-x86_64.tar.gz
sudo ./fxsound-linux-0.4.0-x86_64/install.sh        # /usr/local unless you name another prefix
```

`/usr/local` is searched for presets alongside `/usr`, and first by a binary installed there, so
nothing needs configuring afterwards and a distribution package's presets under `/usr` never stand
in for the tarball's own.
The script always builds the binary from the tree it sits in first (with `--locked`; a no-op when
the build is current), and refuses to pack one whose `--version` is not the version in
`Cargo.toml`, so a binary left in `target/release` by an older checkout never ends up in a newer
archive.

A prebuilt tarball is attached to each [release](https://github.com/blackixxce12/fxsound-linux/releases);
check it against the release's `SHA256SUMS` before unpacking. It is this same script's output, built
on Debian 12's glibc so the binary runs on anything newer, and it carries the same `install.sh`. The
archive is a prefix in miniature: `bin/fxsound`, `lib/systemd/user/fxsound.service`,
`share/dbus-1/services/` (D-Bus activation), `share/applications/`, `share/icons/hicolor/` (the
app icon and the tray's three status icons), `share/fxsound/presets/`, `share/man/man1/fxsound.1`,
`share/metainfo/` and `share/doc/fxsound-linux/` (README, CHANGELOG, LICENSE, the licences of
the bundled RNNoise code and Noto faces, the Hyprland rules and the autostart entry).
`install.sh` copies all of that into the prefix and rewrites the unit's `ExecStart` and
`ExecStop`, and the activation file's `Exec`, to `<prefix>/bin/fxsound`; it warns if the prefix
is anything other than `/usr` or `/usr/local`, because those are the only two the binary searches
for presets. On an older distribution than Debian 12, build from source instead.

### What lands where

| Path | What |
|---|---|
| `/usr/bin/fxsound` | the binary |
| `/usr/share/fxsound/presets/Factsoft/`, `BonusPresets/` | the 34 factory and bonus `.fac` presets |
| `/usr/share/fxsound/presets/Input/` | the 13 voice presets, in TOML |
| `/usr/share/applications/com.fxsound.FxSound.desktop` | the launcher entry |
| `/usr/lib/systemd/user/fxsound.service` | `systemctl --user enable --now fxsound` |
| `/usr/share/dbus-1/services/org.fxsound.FxSound.service` | starts FxSound, through that unit, for a D-Bus call |
| `/usr/share/icons/hicolor/*/apps/fxsound.png` | the icon |
| `/usr/share/icons/hicolor/*/apps/com.fxsound.FxSound.png` | the same icon under the application id, the name the notifications carry |
| `/usr/share/icons/hicolor/scalable/status/` | the tray's three status icons |
| `/usr/share/man/man1/fxsound.1` | the manual page: every option, the status document and the D-Bus interface |
| `/usr/share/metainfo/com.fxsound.FxSound.metainfo.xml` | what GNOME Software and Discover show |
| `/usr/share/doc/fxsound-linux/` | the Hyprland rules and the autostart entry |
| `/usr/share/licenses/fxsound-linux/` | the AGPL, and the licences of the RNNoise code and the Noto faces built into the binary (the `.deb` carries them in `/usr/share/doc/fxsound-linux/copyright`) |

FxSound started by the user unit — enabled, or by a D-Bus call such as a status bar's — runs in the
systemd user manager's environment, not the compositor's. Where the compositor does not import
`WAYLAND_DISPLAY` into it (sway, and Hyprland without uwsm), that FxSound has no display: it runs in
the tray, and asking for its window brings a notification that it could not be opened instead. Import
it when the compositor starts, with `exec dbus-update-activation-environment --systemd
WAYLAND_DISPLAY DISPLAY` in sway (`exec-once = ...` in Hyprland).

There is no AppImage or Flatpak. A Flatpak would be actively counterproductive here: the whole
point of the app is to own a node in your PipeWire graph and drive your real output device, and the
sandbox exists to prevent exactly that.

## Running under Hyprland

The window is frameless and has a fixed design size, so it wants to float rather than tile. Copy the
rules from [`packaging/hyprland.conf.example`](packaging/hyprland.conf.example) into your
`hyprland.conf`:

```conf
windowrule {
    name = fxsound
    match:class = ^(com\.fxsound\.FxSound)$
    float = true
    border_size = 0
}
```

That is the block syntax Hyprland 0.53 and newer read: the release that rewrote window rules also
dropped `windowrulev2` (a config error now, not a warning) and renamed the window rule's `no_border`
to `border_size = 0`; the file passes `Hyprland --verify-config` on 0.56.2. The example keeps the
old `windowrulev2` lines as comments for 0.52 and older, and the `hl.window_rule` / `hl.bind`
spelling for the Lua config that Hyprland prefers over `hyprland.conf` from 0.55 on.

Closing the window never quits. The ✕ button and the compositor's own close request (`killactive`,
usually bound to `Super+Q`) hide the window while the audio keeps processing; the app lives on in
the system tray, and the tray's **Open** or `fxsound --show` bring the window back. **Exit** in the
tray menu or `fxsound --quit` are the only ways out. This is what the Windows build does too — with
the difference that a Wayland client cannot unmap its own toplevel, so "hide" here really destroys
the window and recreates it on demand. The minimise button hides the window into the tray as well
while a tray icon is there to come back from (Hyprland and sway have no minimised state to put it
in); on a desktop with no tray, such as GNOME without an AppIndicator extension, it minimises the
window as the Windows build does, instead of making it vanish, and `fxsound --show` or launching
FxSound again brings a minimised window back (on Wayland as a fresh window in its place, since an
application cannot take its own window out of the minimised state there). FxSound quit with its
window in the tray starts in the tray next time, unless it is started with `--show` or no tray icon
comes up to be in, when the window comes up minimised after a few seconds.

Fullscreen (Hyprland's `fullscreen` dispatcher, Super+F in many configurations) and maximised
windows scale the fixed design up to fit the monitor and centre it; the app never draws into a
corner of an oversized surface. A tile narrower or shorter than the design — a tiling
compositor's column, such as niri's default half-screen one — scales it down to fit instead of
cutting it off, to half size at the least. The hamburger menu and the drop-down lists are drawn
inside the window, and the window does not grow for them: the menu scrolls under the hamburger,
and a list scrolls between the title bar and the window's bottom edge, over its own box when there
is no room under it — in the Lite view, four rows of the menu and two and a half of a list show at
a time. FxSound gives the compositor each view's size as the window's smallest and largest, so
flipping between Pro and Lite and opening Settings resize a floating window on Hyprland too.

Global shortcuts are the one feature that cannot work the way it does on Windows. A Wayland client
is not allowed to grab keys it does not have focus for — that is a deliberate security property of
the protocol, not a gap. The same file binds them in the compositor instead, which reaches the
running instance through its control socket. It uses Super+Alt, which few desktops and no
applications use, because a compositor binding takes its keys from every application — the Windows
build's own Ctrl+Shift+Q is how Chromium quits:

```conf
bind = SUPER ALT, P,            exec, fxsound --toggle-power
bind = SUPER ALT, bracketright, exec, fxsound --next-preset
```

In sway the same is `bindsym $mod+Mod1+p exec fxsound --toggle-power`. None of these raises the
window: on the command line only `--show`, `--view` and a bare `fxsound` do, and every option that
sets something — `--preset=Gaming` from a keybind included — does it without pulling the window over
what you are doing. That holds when the keybind is what starts FxSound, too: it starts in the tray
(with no tray icon after a few seconds, minimised), and `--view` at a start sets the layout without
showing the window unless the tray state or `--show` says so.

## Scripts, status bars and D-Bus

A running FxSound can be driven and watched without its window. `man fxsound` has every option, the
whole status document and the D-Bus interface; what follows is the short version.

Ask a running instance what it is doing, as one JSON document — both lanes, their devices and
presets, the microphone's readouts, every preset, every device, every band with its range, and the
applications:

```bash
fxsound --status --json | jq '.output'
```

It carries every key the Windows build's `--status` writes, too, so a client written for either
reads it. For a bar, `fxsound --watch --json` is better than polling that: it stays connected,
opens with the whole status and prints one JSON object per line as things change — `power`,
`preset_changed`, `device_changed`, `audio_state`, `notice`, `app_routed` and the rest — until
FxSound quits. It never starts FxSound. A Waybar module that shows the speakers' preset, or *off*,
is a few lines of `jq` over it, saved as `~/.config/waybar/fxsound.sh` and made executable:

```sh
#!/bin/sh
fxsound --watch --json | jq --unbuffered -c -n '
  foreach inputs as $e ({on: false, preset: ""};
    if $e.event == "status" then {on: $e.status.power, preset: ($e.status.output.preset // "")}
    elif $e.event == "power" then .on = $e.on
    elif $e.event == "preset_changed" and $e.direction == "output" then .preset = $e.name
    else . end;
    if $e.event == "quit" then {text: ""}
    else {text: (if .on then .preset else "off" end),
          tooltip: "FxSound: \(.preset)",
          class: (if .on then "on" else "off" end)} end)'
```

```jsonc
"custom/fxsound": {
    "exec": "~/.config/waybar/fxsound.sh",
    "return-type": "json",
    "restart-interval": 5,
    "on-click": "fxsound --toggle-power",
    "on-click-right": "fxsound --toggle-window"
}
```

The module hides while FxSound is not running, and `restart-interval` picks the stream up again
once it is. `--meters` adds the microphone's readouts, at most four times a second.

The same commands are a D-Bus interface, `org.fxsound.FxSound` at `/org/fxsound/FxSound` on the
session bus, for anything that would rather hold a connection than start a process:

```bash
busctl --user call org.fxsound.FxSound /org/fxsound/FxSound org.fxsound.FxSound SetPreset s Music
busctl --user call org.fxsound.FxSound /org/fxsound/FxSound org.fxsound.FxSound \
    Apply as 2 -- --preset=Music --set_effect=bass:7.5
busctl --user --auto-start=no call org.fxsound.FxSound /org/fxsound/FxSound \
    org.freedesktop.DBus.Properties Get ss org.fxsound.FxSound Power
busctl --user monitor org.fxsound.FxSound
```

Each method does what its option does — `TogglePower`, `SetPreset`, `SetOutput`, `SetInput`,
`SetNoiseSuppression`, `SetAppPreset`, `GetStatus` and the rest — and `Apply` runs a whole command
line and answers with what it printed. Properties carry the power, the edited lane's preset, both
lanes' devices and the edit direction, and signals say when they change. A call with no FxSound
running starts it in the tray, through the systemd user unit, so anything that only reads — a bar
module above all — should ask the bus not to (`busctl --auto-start=no call`, or the `NO_AUTO_START`
flag), or it brings FxSound back seconds after you quit it. Read a property with `call` and
`org.freedesktop.DBus.Properties Get`, as above: `busctl get-property` and `busctl introspect`
start FxSound whatever `--auto-start` says (systemd 262). The command line keeps to its own socket
and works without a session bus.

`fxsound --self-test` checks an installation without a display, a session or a sound server —
presets, settings, both chains run offline, the desktop entry, icons, unit, activation file, man
page and metainfo — and never touches a running FxSound. Every package's CI job runs it.

## How the audio path works

```
   your apps
       │  play into
       ▼
┌──────────────────┐     ┌─────────────────────┐     ┌──────────────────┐
│  FxSound sink    │────►│  music chain        │────►│  your real sink  │
│  (virtual, ours) │     │  EQ → effects → lim │     │  (you choose it) │
└──────────────────┘     └─────────────────────┘     └──────────────────┘

┌──────────────────┐     ┌─────────────────────┐     ┌──────────────────┐
│  your real mic   │────►│  voice chain        │────►│  FxSound source  │
│  (you choose it) │     │  denoise → … → lim  │     │  (virtual, ours) │
└──────────────────┘     └─────────────────────┘     └──────────────────┘
                                                            │  record from
                                                            ▼
                                                        your apps
```

FxSound publishes its own sink. Everything written there is processed and rendered to whichever real
device you pick in the app. That is the same shape as the Windows virtual driver, implemented with
PipeWire nodes instead of a kernel driver — which also means uninstalling is `pkill fxsound` and
your audio comes straight back.

A microphone is a second lane beside it, not a switch: a capture stream from the microphone you pick
runs [the voice chain](#the-microphone-chain) and feeds a virtual source, `FxSound (Input)`, that
applications record from. Each lane has its own device, preset, nodes and claim on the session
default, and either can be set to *Off* while the other runs; the one power button covers both.

Neither lane keeps anything awake for nothing. The stream to your speakers sleeps while no
application plays into FxSound, and the microphone is opened only while something records from
`FxSound (Input)`, the calibration runs or the Pro view shows the microphone's meters.

The volume the desktop shows for `FxSound (Output)` is kept per output device, so a level set for
quiet laptop speakers does not come back at full on headphones, and it is applied after the chain
rather than in front of it, where the volume levelling would win part of a turn-down back.

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
in ─► master gain · balance ─► 31-band graphic EQ ─► volume levelling
                               └─── skipped while the EQ is off ───┘
   ─► Fidelity ─► Ambience ─► Surround ─► Bass ─► Dynamic Boost ─► out
```

The EQ, the master gain, the balance and the volume levelling are one block in the original, whose
switch the equalizer's is. Here the master gain and the balance play whatever the EQ switch and the
power button say, so neither switch moves the level or the balance: with the EQ off the curve and
the volume levelling are skipped and the effects run, and power off bypasses everything but the
master gain and the balance. On 5.1 and 7.1 the balance turns down a whole side — front, side and
rear — and leaves the centre and the subwoofer alone.

| Effect | Algorithm |
|---|---|
| Fidelity | 2nd-order Butterworth high-pass at 1745.5 Hz into a sine waveshaper that deliberately folds over |
| Ambience | Dattorro/Lexicon-224-style figure-of-eight plate reverb |
| Surround | Mid/side gain widener, side ×(1+3i), mid ×(1−0.3i) |
| Bass | One parametric peaking biquad, 90 Hz, Q 2.5, 0–15 dB |
| Dynamic Boost | ~1.6 s RMS auto-gain plus a 0.75 ms look-ahead brick-wall limiter, linked across the channels, that holds its gain 20 ms before letting go |

Total added latency is the limiter's look-ahead: 0.75 ms.

Every stage follows the channel layout the device reports rather than assuming the first two
channels are the front pair.

## The microphone chain

A microphone does not get the music chain pointed at a different input. Music processing exists to
make music sound *bigger* — reverberation, a bass lift, a stereo widener — and every one of those is
the opposite of what a voice wants. So a microphone gets a chain of its own:

```
mic ─► RNNoise ─► de-reverb ─► high-pass ─► gate ─► 10-band EQ ─► de-esser ─► compressor ─► makeup ─► limiter ─► out
```

| Stage | What it does |
|---|---|
| RNNoise | Recurrent-network denoiser. Off unless the preset asks. ~44 dB off a desk microphone's hum-under-hiss; on undifferentiated white noise, ~1 dB — it separates speech from noise, and white noise gives it nothing to separate. Three levels, *Mild*, *Medium* and *Strong*, set how deep it may cut, how hard it holds down what is not a voice and how much of the voice it spares. A stereo microphone is denoised as *Mono*, *Linked stereo* (one analysis, the same mask on both sides, so the talker stays where they are) or *Independent* |
| De-reverb | Suppresses a bare room's late reverberation, *Mild*, *Medium* or *Strong*. Off unless the preset or Settings asks |
| High-pass | Butterworth, 2nd or 4th order. Desk rumble and plosives sit ten to twenty dB above the voice below 150 Hz; left in, they hold the gate open through every pause |
| Gate | Downward expander with a threshold, a ratio, a floor, hold, and peak or RMS detection — it turns the room down rather than switching it off. A preset can also hold it open on RNNoise's voice probability, so a quiet consonant gets through and a keyboard does not |
| Equalizer | The same graphic EQ the output chain uses, ten bands unless you pick five to 31 |
| De-esser | Split-band, fourth-order Linkwitz–Riley crossover, acting only on the high band. *Adaptive* takes the band from what the microphone really carries — a 16 kHz Bluetooth headset has nothing above 8 kHz — and stands aside where there is no sibilance band at all |
| Compressor | Threshold, ratio, soft knee, attack, release, peak or RMS detection |
| Makeup | Applied after everything that measures, so a preset's thresholds mean what they say |
| Limiter | 1 ms look-ahead, linked across the channels with a 20 ms hold, always running. It is the only stage that cannot be switched off: makeup gain is the one control here that can push a sample past full scale |

The order is not a preference — each position is argued, with its reason, at the top of
[`crates/fxsound-dsp/src/input/chain.rs`](crates/fxsound-dsp/src/input/chain.rs). Denoising goes
first because everything below it measures a level. The gate goes before the equalizer so that what
it measures is the microphone and not the preset's own presence lift. The de-esser goes before the
compressor, because a compressor in front would ride the sibilant and duck the word behind it. A
voice preset may still name another order, `chain = "podcast"`, `"broadcast"` or `"streaming"`, each
argued in [`crates/fxsound-dsp/src/input/processor.rs`](crates/fxsound-dsp/src/input/processor.rs).

**Echo cancellation** is PipeWire's own WebRTC canceller, loaded into FxSound: the microphone and
what the speakers actually play go in, and the voice chain records from the echo-cancelled result,
so a call on speakers does not hear itself. It runs only while something records from
`FxSound (Input)`, and if the WebRTC module is missing the microphone keeps working and the window
says so.

Added latency is the limiter's 1 ms, plus 20 ms for RNNoise and 10 ms for the de-reverb while they
run. All of it is published to PipeWire and kept current, so a recording application is never told a
figure that has since changed. The capture stream asks for 48 kHz whatever the microphone runs at —
RNNoise exists at 48 kHz and nowhere else — so a voice preset means one thing on every device.

**Settings ▸ Microphone** holds what is not a preset: the noise-suppression level and the
denoiser's channels, each either *Preset* or one choice that every voice preset then runs at; the
de-esser mode and de-reverb, which can ask for more than a preset does but never less; echo
cancellation; the calibration; and the microphones' priority list, each with its own voice preset.
`--noise-suppression` sets the level from a script.

**Calibrate microphone…** on that page listens to the microphone before the chain touches it:
three seconds of the room, five of normal speech and two of loud speech. From the floor, the level
and the peaks it proposes a high-pass, a gate, a compressor, makeup, a ceiling and a
noise-suppression level, and *Apply* saves them as a voice preset of the microphone's own, named
after it. A Bluetooth headset is woken into its headset profile first, and the room is timed only
once it sends sound.

### Voice presets

Thirteen ship, in TOML rather than `.fac`: a `.fac` is a byte-for-byte contract with the Windows
build and has nowhere to put a gate threshold. A stage that is switched off has no table in the
file, so there is no way to ship a full set of numbers that nothing reads.

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
| Gaming Headset | A gaming headset's boom with a keyboard and fans behind it: denoised in linked stereo, gated on the voice, the adaptive de-esser, kept bright |
| Noisy Room | An open room with a fan or traffic in it: the denoiser at full strength, then a gate that only hears the voice |
| Mechanical Keyboard | A desk microphone beside a clicky keyboard: a fast peak gate between words, the denoiser at full strength |

The picker shows the voice set in front of a microphone and the `.fac` set in front of a speaker,
and never mixes them. Your choice is remembered **per direction**, so a preset picked for a
microphone does not follow you back to your speakers. Voice presets are edited, saved, renamed,
deleted, imported and exported like the `.fac` ones, and yours live in
`~/.local/share/fxsound/presets/Input/`. The reasoning behind every number in the shipped files is
in `docs/input-presets-decision.md`.

It is also remembered **per device**, in both directions: every output keeps the `.fac` preset and
every microphone the voice preset it was last used with, and brings it back whenever FxSound moves
to it again — picked in the window or the tray, with `--output`, `--input`, `--next-output` or
`--next-input`, or chosen by the priority list when it is plugged in. A device used for the first
time leaves the preset alone. The rows of Settings ▸ Audio's *Output Device Preference* and of
Settings ▸ Microphone's *Input Device Preference* each have a combo that sets that device's preset
ahead of time; set on the device in use, it takes over at once. A renamed preset takes its devices
with it; a device whose preset was deleted shows *Select preset* and keeps whatever the lane is
running when it comes back.

While a microphone is selected the window says so rather than pretending: the five effect sliders
are drawn disabled with the reason underneath, and an equalizer band the device's sample rate cannot
carry is struck through instead of left looking live. Along the bottom of the panel the chain reads
out live — the denoiser's reduction, the noise floor, the voice probability, echo cancellation,
de-reverb, the gate, the compressor and the de-esser — and a stage with nothing to report reads
*off*, a dash, or *unavailable* with the reason, never a stale number. Turned over, the effect
column shows only what the voice chain has: the band count, the filter width, and the preset's
makeup gain in the master gain's place.

## Presets

`.fac` files are read and written in the original's format — a line-based text format, despite the
extension. Every preset that ships with FxSound round-trips byte-for-byte, so presets copy in both
directions between this port and the Windows build.

| | Path |
|---|---|
| Factory presets | `/usr/share/fxsound/presets/Factsoft/`, `BonusPresets/` |
| Voice presets | `/usr/share/fxsound/presets/Input/` |
| Your presets | `~/.local/share/fxsound/presets/`, voice presets under `Input/` |
| Unsaved edits | `~/.local/share/fxsound/presets/AutoSave/` |
| Settings | `~/.config/fxsound/settings.toml` |
| Applications and their presets | `~/.config/fxsound/apps.toml` |

The flip button at the top of the effect column turns it over to the equalizer's own controls, as in
the original since 1.2.12: the band count, the master gain, the volume leveling, the filter width,
the balance and Restore Defaults. Each slider has the original's range and step, except that the
master gain and the balance step by 1 dB where Windows steps by 2, so every value `--master_gain`
and `--balance` take can be set from the window too. The volume leveling reads its 0 to 4 amount
without the "dB" Windows puts after it. A right-click puts any slider back to its default, the
effects included (which it switches off), and every slider's tooltip says so. A press on a slider's
thumb moves nothing until the pointer does, where Windows jumps to the pointer and can move the
gain or the balance by a step, and one wheel notch is one step. Every title-bar button and the flip
button say on hover what they do, and *Hide help tips* hides that too.

With the power off the sliders are grey but still show their values, and a preset can still be
picked, from the window, the menu or the tray, as from the command line; switching back on plays it.

An effect slider has the original's eleven positions, but a preset stores 128 values: one between two
positions is shown with its decimal (General's Surround reads 1.6), a press on the thumb does not
move it, a plain arrow key or wheel notch takes it to the next position that way (Surround 1.6 to 2
or 1), and **Shift** with the arrow keys, the wheel or a drag steps one stored value at a time, so
loading a preset and saving it changes nothing. Ambience's positions 1 to 10 run over the values it
can be heard at; on Windows positions 1 to 3 were all but silent.

The equalizer's curve is the response the equalizer really has, filter width and all, where the
Windows build joins the band values with straight lines: bands boosted side by side add up, and a
narrower filter width draws a narrower peak. The first and last band's wheels on five and ten bands
turn both ways, reaching half a band past the ladder (on ten bands 46 Hz and 20 kHz); a preset
exported for Windows has such a band put back at 62.5 Hz or 16 kHz, where its wheels stop, unless,
at «Like FxSound for Windows» = Interface and sound, "Keep the end bands where they are" is ticked
in the Export window (`--export-unshifted`). **Ctrl+Alt**+drag on a band solos it, as Alt+drag does
on Windows: every other band sinks to −10 dB while you listen and comes back when you let go, or
when a preset, the band count or the lane changes under it, and the preset is not marked as changed.
The solo is offered while the equalizer plays, not while it is switched off. A press on a band's
knob moves nothing until the pointer does, as on the sliders, and a band held when a preset, the
band count or the lane arrives from elsewhere stays where the new curve puts it until you let go.

A preset lands on your band count, as in the original since 1.2.11: pick a ten-band preset while on
31 bands and its curve is fitted onto the 31, and changing the band count carries the curve over
instead of flattening it. Both go by frequency, so a boost stays where it was. The band count is
your setting, not an edit to the preset. Restore Defaults puts back the neutral levels and keeps the
band count and the curve. The twenty-band equalizer's bands sit every half octave; a Windows
twenty-band preset is moved onto them band for band, and exported back on the Windows ladder.
Export writes a preset as last saved, never its unsaved edits.

Unsaved edits are kept in `AutoSave/` a minute after the first one and whenever you switch presets
or quit, so a crash loses at most a minute. **Delete Preset** asks first and moves the file to the
desktop's trash, where a file manager can restore it, and its unsaved edits with it: restore both
and the preset is back with its `*`. **Save New Preset** also copies a preset with
no unsaved changes. A new preset's name is cut to the 126 bytes a Windows FxSound reads a name in
(63 Cyrillic letters), and a line break or a tab in it becomes a space. Saving over one of your
presets writes the file it was listed from, whatever that file is called, so a preset 0.3.0 saved
as `Rock:Live.fac` keeps its file, and the unsaved edits 0.3.0 left in `AutoSave/` come back with
it. **Reset presets** in Settings asks first, then discards every unsaved change and keeps your
saved presets. `max_user_presets` in the settings file allows 10 to 1000 presets.

The settings file keeps the original's key names so it can be diffed against the Windows
`FxSound.settings`. Settings and presets are written durably — temporary file, fsync, rename — so an
interrupted save cannot truncate what was there. A settings file that does not load (a typo, a
comment saved in a legacy encoding, permissions) is moved aside to `settings.toml.bad`, or
`settings.toml.2.bad` and so on, never over an earlier one, before the defaults are saved. Keys this
version does not know, such as those a later version wrote, are kept and written back as they
were; 0.4.0 drops them on its next save. A settings file or preset that is a symbolic link, as GNU
Stow or chezmoi leave them, stays one: the file it points to is the one replaced. A link to a
read-only file, or to one in a directory you may not write, is never saved through, so every save of
that file fails. home-manager's default links into the Nix store are such links; link the file with
`mkOutOfStoreSymlink` to let FxSound save it.

## A preset per application

Every application that plays or records through FxSound is remembered, and each can have a preset of
its own on either lane: a game on *Gaming*, a browser on *Volume Boost*, a voice chat's microphone
on *Headset*, all at the same time. One chain cannot run two presets at once, so FxSound gives such
an application a pair of nodes of its own on the same device, `FxSound (Output) · Gaming` for one,
running its preset, and moves the application's stream onto it the way a volume mixer moves a stream
— through the stream's target in PipeWire's `default` metadata, which WirePlumber honours. The rest
of the session keeps the lane's preset. Up to four presets of one lane run this way at a time; an
application past that stays on the lane's preset, and the window says so.

**Settings ▸ Applications** lists every application FxSound has seen, the ones running now first,
each with a preset combo for what it plays, what it records, or both; *FxSound's preset* has it
follow the lane again, and the ✕ forgets it. The Pro view's preset combo says on hover which
applications are running on presets of their own. From a script:

```bash
fxsound --app-preset='bf6.exe=Gaming' --app-preset='firefox=Volume Boost' \
    --app-input-preset='com.discordapp.Discord=Headset'
fxsound --app-preset='firefox=default'     # back to the lane's preset
fxsound --list-apps
```

An application is named by its Flatpak id, its program or the name it gives itself, and one FxSound
has never seen gets its preset the first time it plays. D-Bus has the same as `SetAppPreset` and
`ListApps`, and `--watch` reports every move as `app_routed`. The choices live in
`~/.config/fxsound/apps.toml`, which keeps the 500 applications heard most recently, forgetting
those that only follow the lanes first.

With the power off none of them is moved: every application plays and records through the system's
default devices, and power on takes them back onto their presets. A stream you move by hand onto
one of these nodes is moved back to where its application's preset puts it — give it a preset
instead — while a stream moved anywhere else, or a recorder pointed at such a node's monitor, stays
where you put it.

## Deliberate differences from the Windows build

Each of these is a considered decision, not an oversight:

- **The sound is not quite the Windows build's.** Since 0.4.0 defects of the original DSP are fixed
  rather than reproduced, so a preset plays close to, not exactly, as it does on Windows. Bass
  driven into the limiter comes out clean: the limiter holds its gain for 20 ms and moves the
  channels together, where it breathed within each bass cycle — up to 18.6 % distortion on a limited
  bass tone with the shipped presets, at most 0.016 % now — and dragged the stereo image towards the
  other side. Loud, finished masters play 1.4 to 3.5 LU quieter for it, because the limiter no
  longer squeezes the bass into loudness. Dynamic Boost at 0 lifts nothing and judges loudness from
  both front channels; the volume levelling keeps deep bass clean, reacts at one speed whatever
  buffer size PipeWire runs at and lifts the subwoofer too; the balance on 5.1 and 7.1 turns down a
  whole side; an effect or band switched back on starts clean; the twenty-band equalizer's bands sit
  every half octave; a slider, a preset change or the EQ switch glides over about 20 ms instead
  of clicking; the power button, and on a microphone a preset change or a stage switched in or out,
  dips out and back in over 20 ms instead of stepping; and a sound moved onto FxSound in the middle
  of a note — every application, when the power comes back on — fades in instead of starting at
  full level. No shipped preset was re-voiced, and each genre preset still ranks where it did for
  its genre.
- **No update check and no telemetry.** Nothing contacts the network; updates come from your package
  manager. The Windows keys for the update check, the hotkey chords, the window position and
  always-on-top are read from an older `settings.toml` without complaint and no longer written back.
- **Deterministic preset ordering.** The original lists presets in filesystem-glob order, which is
  arbitrary. This port lists the factory presets first, the numbered ones in the vendor's order and
  the rest by name, then the user's own presets by name; a user's copy of a factory preset keeps
  the factory one's place.
- **Frame-rate-independent visualizer decay.** The original decays its bars once per timer tick;
  this port decays by elapsed time, so the animation looks the same at 60 and 165 Hz.
- **Q multiplier no longer resets the band layout.** Changing the filter width in the original
  silently discards preset-supplied band frequencies and resets the sample rate to 44100 until the
  next buffer. That is a bug; this port only redesigns the coefficients.
- **Presets work with the power off.** The original greys out its preset list, its menu's preset
  items and its tray's preset menu while processing is off, and its command line ignores
  `--preset`, `--save_preset` and the rest. Here all of them work either way, and a command is
  refused exactly where the hamburger menu greys the item out — an overwrite or rename of a
  factory preset, a rename with unsaved changes, a name already taken, the user-preset limit —
  with the reason on stderr, exit status 1, and `org.fxsound.FxSound.Error.Refused` on D-Bus.
- **Global hotkeys live in the compositor.** See above.
- **Window position is not restored.** Wayland gives a client no way to place its own toplevel, so
  the compositor places the window.
- **The command line raises the window only when asked to.** On Windows every option but `--status`
  shows and raises the window; here only `--show`, `--view` and `fxsound` with no options do, so a
  keybind or a script sets a preset, the power or an effect without the window jumping in front —
  and a line of such options that starts FxSound starts it in the tray.
- **The command line says what it will not do.** Two preset options on one line (`--save_preset=A
  --preset=B`, which Windows reads as `--preset=B` alone), a new preset name that is nothing once the
  characters a `.fac` name cannot hold are gone, and a `--language` FxSound has no translation for
  are parse errors; a band list naming a band the equalizer does not have (`--set_band_gain=12:2` on
  ten bands), or a `--set_band_freq` outside the band's range, is refused whole with the band named.
  Windows ignores all of them without a word. `--language` also takes the ISO codes Windows spells
  its own way (`uk`, `bs`, `nb`) and locales.
- **Every option works when it starts FxSound.** Windows drops the band lists, `--set_effect` and
  the preset commands (`--next-preset` from a keybind included) when there is no FxSound running
  for them; here the start carries them out on the preset it selects. `--next-output` and
  `--next-input` have no device to step from until FxSound runs, so with none running they are
  refused and nothing starts.
- **Two device menus in the tray.** The playback devices are under *Playback Device Select* and the
  microphones under *Recording Device Select*, and a long device name is shortened in its middle,
  where Windows cut every name after 30 characters and PipeWire's names of one card's outputs all
  looked alike.
- **No Donate button, no update check, no bonus-preset download, no Help center.** The heart in
  the title bar and the Donate items in the menu and the tray are gone: this fork is not the
  upstream developers' product and must not solicit money for them. "Check for updates" and the
  "Automatic updates" toggle need a network the package never uses, the bonus presets ship inside
  the package, and Settings ▸ Help keeps only the version and a **Changelog** that opens the
  bundled `CHANGELOG.md` in-app instead of the upstream website. "Always On Top" is also absent:
  winit ignores window levels on Wayland, and a control that does nothing is worse than none.
- **Translated, from the original's own tables.** The 29 JUCE `LocalisedStrings` files embedded in
  the Windows binary (`assets/translations/`, extracted from its `BinaryData.cpp` as of 1.2.16.0,
  which added Bulgarian; Hungarian was declared but never shipped) are embedded here and looked up
  by the same English keys the C++ passes to `TRANS`, read as JUCE reads them, so the lines the
  Windows tables leave unclosed translate here as they do there. The language follows the desktop session (`LC_ALL`/`LC_MESSAGES`/`LANG`)
  unless one is picked in Settings ▸ General or with `--language <code>` (`--language system`
  returns to following the desktop). The switch lists English and then every language by its own
  name, in alphabetical order, where Windows used an order of its own and called three of them by
  the wrong word (Turkish "Türk", Thai "แบบไทย", Czech "Česky"). Strings this port added are in
  `assets/translations/port/`. Right-to-left scripts render left-to-right — egui has no bidi.
- **Desktop notifications** for preset, output and power changes, the way the original's tray
  balloons announce them, through `org.freedesktop.Notifications`; *Hide notifications* in Settings
  silences them. What has to be seen — where the window went, a lost output, a refused save, the
  power toggled from a keybind, a change made while the window is hidden or minimised — is sent at
  normal urgency; a preset or an output picked in the window is sent at low urgency, which GNOME
  files in the message list without a banner. Every notice stays in GNOME's list until it is
  dismissed or FxSound quits.
- **FxSound takes the session default automatically — politely.** Picking an output device makes
  `FxSound (Output)` the default sink so every application plays through it without any manual
  routing; the previous default is remembered first and handed back on exit or when a lane is
  switched `Off`. Only `default.configured.audio.*` is ever written; `default.audio.*` stays
  WirePlumber's.
- **Power off takes FxSound out of the path; it is no longer a bypass alone.** 0.3.0 kept every
  application playing through FxSound's nodes with the effects switched off. Now, as in the Windows
  build since 1.2.6, power off also hands both session defaults back to the real devices, so sound
  no longer passes through FxSound at all (no added latency, and the desktop's own device switcher
  works), and power on takes them again. That goes for applications with a preset of their own
  too: while the power is off none of them is kept on its route, and each plays and records
  through the system's default devices like everything else, until the power comes back on and
  takes them onto their presets again (a stream that holds on to its preset's node by itself stays
  there, unprocessed). While it is off, the device lists show the system's
  default device, which is where the sound goes; a device picked then is the one FxSound takes
  over when the power comes back on.
- **The device priority list can be told to step aside.** Settings ▸ Audio's list (and Settings ▸
  Microphone's list of microphones, a port addition) picks the device as the Windows build's does:
  every device seen joins it, at the bottom or, with *Prioritize new output devices*, at the top.
  *Follow the system's default device*, which the Windows build does not have (its issue #629),
  hands that choice back to the desktop's sound settings and keeps the list for later: a device
  picked there moves the lane and FxSound stays the default, even after a device was picked in
  FxSound. A device that
  is not connected has a ✕ beside it that forgets it and the preset it remembers, and
  `fxsound --forget-device=NAME` (D-Bus `ForgetDevice`) does the same from a script. With no
  FxSound running it forgets the name from the settings file and exits, without starting FxSound.
- **Mono outputs are accepted.** The Windows build refuses an output with fewer than two channels,
  a workaround for a Windows driver (`sndDevices.h:32-39`). Here a Bluetooth headset in its
  hands-free (call) profile or a mono USB headset is an output like any other, and PipeWire mixes
  FxSound's stereo down for it. And a device plugged in at the moment another goes — a USB DAC
  swapped for another, a headset changing profile — is noticed as a new device, which the Windows
  build misses (its open PR #532).
- **A Bluetooth microphone wakes only when something records.** FxSound holds a microphone open
  only while an application records from `FxSound (Input)`, the calibration wizard runs, or the Pro
  view shows the microphone's meters, so a headset is not switched to its call profile — mono,
  16 kHz — just because its microphone is picked. The picker says so on hover, and using one
  headset for both lanes says what it costs.
- **Suspend mutes, resume starts clean, nothing waits.** FxSound listens for logind's
  `PrepareForSleep` on the system bus: going to sleep mutes both lanes, and on resume their filters
  are cleared before they are unmuted, as the Windows build does from its suspend notification. It
  never holds a sleep inhibitor, as the Windows build decided too; without a system bus it simply
  does not listen.
- **A microphone gets a processor of its own, beside the speakers'.** The Windows build has no input
  processing at all. Here the device lists are split into *Output* and *Input*, and choosing a
  microphone builds a second lane — a capture stream feeding a virtual source, `FxSound (Input)`,
  which becomes the default microphone — running [the voice chain](#the-microphone-chain) rather
  than applying reverberation and a bass lift to someone's speech. The speakers' lane keeps running;
  either lane can be set to *Off* on its own.
- **System-visible names follow the system locale.** The node descriptions shown by pavucontrol and
  desktop volume widgets are `FxSound (Вывод)` / `FxSound (Ввод)` on a Russian desktop,
  `FxSound (Ausgabe)` / `FxSound (Eingabe)` on a German one, and so on for the thirteen languages
  the original ships; English otherwise. Node *names* (`fxsound_sink`, `fxsound_source`, …) are
  fixed ASCII, because they are what gets written into metadata.

The original's painting slips are fixed rather than reproduced: the slider fill no longer overshoots
its track by 8 px, the balance gradient no longer ends 8 px early, and the lit slider thumb is drawn
at its 16 points instead of a quarter of that (`widgets::slider::Fidelity::Faithful` keeps the first
two for comparison). In the light theme the power-off spectrum and a bypassed equalizer go a grey
that can be seen instead of white, the Settings rule and the menu's edge get a colour that shows on
the light background, and Settings' tab captions are set smaller where a translation would run
past the rule.

## Implementation status

| Component | Tests | State |
|---|---:|---|
| `fxsound-core` — shared types, scales, settings, the application store, translations | 232 | complete |
| `fxsound-preset` — `.fac` reader/writer, TOML voice presets, preset stores, the trash | 108 | complete |
| `fxsound-dsp` — biquads, graphic EQ, five effects, limiter, leveller, spectrum, engine, voice chain | 505 | complete |
| `fxsound-rnnoise` — RNNoise, vendored from `nnnoiseless`, with an in-place reset and its band gains exposed | 15 | complete |
| `fxsound-ui` — palettes, geometry, assets, widgets, views, dialogs | 568 | complete |
| `fxsound-audio` — PipeWire backend: two lanes, per-application routes, echo cancellation | 616 | complete, verified on a live PipeWire 1.6 session; its graph tests run a private daemon |
| `fxsound-app` — controller, CLI, IPC, D-Bus, event stream, tray, notifications, window shell | 886 | complete |

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
original's items, and picking a microphone moves the session default source to **FxSound (Input)**
and hands it back afterwards.

The audio crate's graph tests start a PipeWire daemon of their own, with synthetic speakers and a
microphone, and watch it from outside with `pw-dump` and `pw-metadata`: both lanes' nodes at once
and a tone through each reaching only its own, a lane detached on its own, both defaults taken and
handed back, the speakers sleeping while nothing plays, the echo canceller put in front of the
microphone and holding nothing awake while nothing records, a headset's sink vanishing through a
profile switch and linked again, and the priority list choosing the device.

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

`man fxsound` ([`packaging/fxsound.1`](packaging/fxsound.1) in the tree, `man -l
packaging/fxsound.1` to read it there) is the reference for using it: every option and its exit
status, the `--status --json` document key by key, the `--watch` events, the D-Bus methods,
properties and signals, and the files FxSound reads and writes.

[`docs/0.5.0-windows-parity.md`](docs/0.5.0-windows-parity.md) is the contract of «Like FxSound
for Windows»: its levels (three in 0.5.0; Everything, which hides the port's own features, comes
later), which of the port's changes and features each one reverts or hides and which none ever
does, and the setting, option, D-Bus members and status keys that carry it.
[`docs/0.5.0-dsp-inventory.md`](docs/0.5.0-dsp-inventory.md) lists every change to the output
lane's sound since the engine before the 0.4.0 audit, with the level each one belongs to, and
`scripts/windows-parity-bitexact.sh <commit>` holds the output lane to another build of itself bit
for bit — every shipped preset as the application plays it, on 10, 20 and 31 bands; CI holds it to
`v0.4.0`, and with `--compat=windows` holds Interface and sound to the engine before the 0.4.0
audit from a cold start. Where the Windows C code is the reference instead (Ambience, the balance
on surround), `scripts/windows-vectors/build.sh` compiles it into the golden vectors the unit
tests hold.

`docs/0.4.0-design.md`, `docs/0.4.0-upstream.md` and `docs/0.4.0-apps.md` record how 0.4.0 was
built: the two lanes, the voice chain's new stages, D-Bus and the event stream; what was taken from
the Windows build's own later releases; and the per-application presets.

`docs/spec/` holds the reverse-engineering specification this port was written against — roughly
20,000 lines covering every subsystem of the original, with citations down to the source line. It is
worth reading before changing any DSP code. `docs/api/` holds verified API cheatsheets for
egui/eframe 0.36 and pipewire-rs 0.10, quoted from the vendored crate sources. The voice chain has
its own reasoning in `docs/voice-presets-research.md`, `docs/input-presets-decision.md` and
`docs/preset-evaluation.md`. [`CHANGELOG.md`](CHANGELOG.md) is what changed and when.

## Licence

AGPL-3.0-or-later, inherited from upstream FxSound.

Two parts of the binary are under other licences, and every package installs their text beside
`LICENSE` (the `.deb` in its `copyright` file): the noise suppression in `crates/fxsound-rnnoise`,
Joe Neeman's `nnnoiseless` port of Xiph's RNNoise with its model, is BSD-3-Clause
(`COPYING.rnnoise`), and the Noto fallback faces are SIL OFL 1.1 (`OFL.noto`, from
`assets/fonts/OFL.txt`).

The bundled artwork and the Gilroy typeface come from the upstream repository and carry their own
terms. Gilroy is Radomir Tinkov's commercial typeface: its files say "All rights reserved" and name
no licence. A redistributable package may need to replace it — its presence in the upstream tree is
not itself a licence to redistribute.

## Credits

FxSound is by [FxSound LLC](https://www.fxsound.com), with major DSP contributions from
[Theremino](https://www.theremino.com). This port would not be possible without their decision to
open the source. It is an independent community project, not affiliated with, endorsed by or
supported by FxSound LLC.
