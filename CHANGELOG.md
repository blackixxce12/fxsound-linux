# Changelog

All notable changes to the FxSound Linux port. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- **«Like FxSound for Windows», in a new Experimental tab of Settings.** A slider of three
  positions — Off, Interface, Interface and sound — with a line under it that says what each
  does, and `fxsound --windows-parity=off|interface|sound` for the same. The level is saved,
  shown in `--status` (`windows_parity`), announced by `--watch` (`windows_parity`) and on D-Bus
  (the `WindowsParity` property and `SetWindowsParity`); what each level changes arrives over the
  rest of 0.5.0 (`docs/0.5.0-windows-parity.md`). Everything, the fourth level, comes in a later
  version: `full` is refused from the command line and D-Bus, `--force` or not, and a
  `settings.toml` that says `full` runs as `sound` and keeps saying `full`, so the later version
  finds Everything again. A tab caption too long for one line, as
  «Экспериментальное» is, takes two, broken where its translation marks the word.
- **At «Like FxSound for Windows» = Interface and sound, Volume Leveling and Dynamic Boost play
  as on Windows.** The levelling steps once a buffer, reads its peak from the bass-light side
  chain, leaves the subwoofer alone and pulls the gain down at the top of a buffer, and Dynamic
  Boost lifts loud material by half a decibel at 0, hears the left channel alone and limits each
  channel on its own, with no hold: the Windows build's arithmetic, sample for sample, on the
  output and on the applications' output routes. The transitions stay FxSound for Linux's.
- **At «Like FxSound for Windows» = Interface and sound, Ambience, the equalizer, the master gain
  and the balance play as on Windows, and a preset is read as there.** Ambience's slider stores
  13, 25 and 38 at its first three positions again, and the reverb plays them as the Windows build
  does; a band below 20 Hz is the flat gain it is there; the master gain and the balance sit
  between the equalizer and the levelling, go off with the equalizer's switch, and with FxSound
  off the master gain alone plays, while the equalizer is on; the balance plays on stereo only. A
  preset of another band count is read onto yours by position, and twenty bands are the Windows
  build's ladder: a twenty-band preset moves to it and back band for band, and nothing is saved
  on the way.
- **«Like FxSound for Windows» moves between Off and Interface and sound without a click, while
  it plays.** What the level changes goes from one sound to the other over 20 ms instead of
  between two samples: the master gain and the balance move to their Windows place and back, the
  subwoofer under Volume Leveling fades between levelled and not, and Dynamic Boost hands its
  limiting over from one design to the other. At −6 dB of master gain and +4 dB of balance the
  switch clicked at −6 to −20 dBFS; it now leaves about −52 dBFS at most, and the same for an
  application with a preset of its own.
- **The Export window can keep the end bands where they are.** From «Like FxSound for Windows» =
  Interface and sound on it has a tick box, "Keep the end bands where they are", and
  `fxsound --export-unshifted[=0|1]` sets the same: a `.fac` then keeps its first and last band
  where you tuned them, instead of moving them into the range FxSound for Windows tunes them in,
  which stays the default.
- **Settings keep what a later version wrote.** A key of `settings.toml` this version does not
  know is written back as it was, so going back a version and up again keeps the newer version's
  settings. 0.4.0 does not keep them: after a downgrade to 0.4.0 its next save drops the keys 0.5.0
  added, `windows_parity` among them.

### Changed
- **`--status --json` is schema 3.** Every key of schema 2 is kept; `windows_parity`,
  `apps_hidden` and `input.hidden` are new. `--status` prints a `windows_parity:` line.

### Fixed
- **A tray that was already running is found.** FxSound took a system tray that was there before
  it started — KDE's, Waybar's, a Quickshell shell's, Noctalia's, GNOME's with the AppIndicator
  extension — for none, until the tray restarted: minimise went to the taskbar, closing into the
  tray was not remembered, and a start with the tray remembered brought the window up minimised.
- **Notifications show on GNOME.** GNOME never shows a banner for low urgency and removed every
  FxSound notification a few milliseconds after it arrived. What you have to see — where the
  window went and how to get it back, a lost output, a refused save, the power toggled from a
  keybind, and any change made while the window is hidden or minimised — now pops up, a preset or
  an output picked in the window goes quietly to the message list, and every notice stays in the
  list until it is dismissed or FxSound quits.
- **The notifications have their icon.** The packages install the application icon under the
  name the notifications ask for, `com.fxsound.FxSound`, as well as `fxsound`.

### Changed
- **Tested on the newest PipeWire as well as the oldest.** Besides Ubuntu 24.04 and its
  PipeWire 1.0, CI runs the tests in an Arch Linux container with the newest PipeWire and
  WirePlumber 0.5, where the tests of WirePlumber's own policy, which Ubuntu's 0.4 has to skip,
  must run. The workflows moved to the Node 24 releases of their actions, run on Ubuntu 24.04
  everywhere rather than on whatever `ubuntu-latest` becomes, and are checked by actionlint.
- **Clicks are measured by the tests.** A steady tone plays through FxSound on a private PipeWire
  with WirePlumber while the power button, an application's own preset, FxSound's and the
  desktop's choice of device, the equalizer and the music and voice presets are switched, and each
  switch's worst click is read from the recording. The equalizer, the presets and the power button
  under sound that stays on FxSound must stay below −40 dBFS. A switch that moves an application's
  sound to another device or node is reported instead, as loud as it is today: −18 to −41 dBFS for
  the power button, as loud as −5 dBFS for an application's own preset or a device chosen in
  FxSound, and −19 to −28 dBFS for the desktop's choice with FxSound off. The moves FxSound makes
  itself are held to −40 dBFS once 0.5.0 makes them quiet. CI shows the measurements in the job's
  summary.
- **The sound is checked bit for bit against 0.4.0.** CI renders every shipped preset at 10, 20
  and 31 bands through this version and through v0.4.0, from a cold start and while a preset, the
  band count and a slider are switched, and requires the same output: identical, or nowhere more
  than −120 dBFS apart. It also renders Interface and sound against the engine before the 0.4.0
  audit, and requires the same output wherever that level is built so far. The offline renderer
  (`process_wav`, with `--compat windows` for Interface and sound) and the blind A/B of
  `scripts/voicing` now play a preset the way the application does, through the application's
  own reading of a preset, and the drift measurement (`preset_drift`) sets Interface and sound
  beside Off with `PRESET_DRIFT_COMPAT=windows`. Moving the level is in the click tests, on the
  output and in an application's own preset.

## [0.4.0] — 2026-09-27

### Added
- **The speakers and a microphone at once.** Output and input are two lanes now, each with its
  own device, preset, nodes and claim on the session default, so `FxSound (Output)` and
  `FxSound (Input)` exist side by side. Picking a microphone no longer takes the speakers down;
  either lane can be switched Off on its own, and one power button covers both. The Pro view has a
  device picker per lane and the Lite view one list holding both, each lane with an Off row; the
  window marks the chain it is editing, and the tray lists both lanes.
- **Noise suppression you can set.** RNNoise runs at Mild, Medium or Strong — how deep it cuts,
  how hard it holds down what is not a voice, how much of the voice it spares — and a stereo
  microphone can be denoised as Mono, Linked stereo (one analysis and the same mask on both
  sides, so the talker stays where they are) or Independent. A voice preset names its own;
  Settings > Microphone and `--noise-suppression` can hold every preset to one level.
- **An adaptive de-esser.** Its Adaptive mode takes its band from what the microphone really
  carries rather than from the stream's rate — a 16 kHz Bluetooth headset has nothing above
  8 kHz to de-ess — and stands aside where there is no sibilance band at all.
- **De-reverb** for a microphone in a bare room: Mild, Medium or Strong, right after the
  denoiser. It adds 10 ms while it runs, published to PipeWire like the rest of the chain's delay.
- **Echo cancellation**, through PipeWire's own WebRTC canceller loaded into FxSound: what the
  speakers actually play is taken back out of the microphone, so a call on speakers does not hear
  itself. It runs only while something records from `FxSound (Input)`. Without PipeWire's WebRTC
  module the microphone keeps working and the window says why echo cancellation is unavailable.
- **A gate that listens for a voice.** A voice preset can open its gate on RNNoise's voice
  probability as well as on level, so a quiet consonant gets through and a keyboard does not.
- **A calibration wizard.** Settings > Microphone > *Calibrate microphone…* listens to three
  seconds of the room, five of normal speech and two of loud speech, measured before the chain
  touches them, and proposes a high-pass, a gate, a compressor, makeup, a ceiling and a noise
  suppression level. Apply saves them as the microphone's own voice preset, named after it. A
  Bluetooth headset is woken into its headset profile first, and the room is timed only once it
  sends sound.
- **The voice chain reads out live** along the bottom of the Pro view: the denoiser's reduction,
  the noise floor, the voice probability, echo cancellation, de-reverb, the gate, the compressor
  and the de-esser. A slot that has nothing to say reads off, a dash or unavailable with the
  reason, never a stale number.
- **Settings > Microphone**, a page for what is not a preset: noise suppression and the
  denoiser's channels, which can override every voice preset; the de-esser mode and de-reverb,
  which can ask for more than a preset does; echo cancellation; the calibration; and the
  microphones' own priority list.
- **Voice presets are presets like any other**: edited, saved, renamed, deleted, imported and
  exported, their unsaved edits kept, where 0.3.0 showed them read-only while its save machinery
  still ran on them. Three new ones ship — Gaming Headset, Noisy Room and Mechanical Keyboard —
  and a hand-written one can name the order of its chain: voice, podcast, broadcast or streaming.
- **A preset per application.** Every application that plays or records through FxSound is
  remembered, and each can have a preset of its own on either lane — a game on Gaming, a browser
  on Volume Boost, a voice chat's microphone on Headset, all at once. FxSound runs that preset
  for it alone, on a pair of nodes of its own on the same device, and moves its stream there the
  way a volume mixer moves a stream; the rest of the session keeps the lane's preset. Settings >
  Applications lists them, running ones first, and `--app-preset`, `--app-input-preset`,
  `--list-apps` and D-Bus do the same from a script. The choices live in
  `~/.config/fxsound/apps.toml`, which keeps the 500 applications heard most recently, forgetting
  those that only follow the lanes first. Up to four presets of one lane run this way at a time.
- **A preset per device, both ways.** Every output keeps the music preset and every microphone
  the voice preset it was last used with, and brings it back whenever FxSound moves to it again:
  picked in the window or the tray, named on the command line, or chosen by the priority list
  when it is plugged in. The rows of Settings > Audio's and Settings > Microphone's priority
  lists set it ahead of time.
- **A D-Bus interface, `org.fxsound.FxSound`.** Everything the command line does is a method,
  and `Apply` runs a whole command line and answers with what it printed; properties carry the
  current state and signals report each change. A call with no FxSound running starts it, in the
  tray, through the systemd user unit. The command line itself stays on the control socket and
  works without a session bus.
- **`fxsound --watch [--json] [--meters]`**, an event stream for Waybar, Noctalia and scripts. It
  opens with the whole status and then prints one line per change — power, preset, device, audio
  state, notices, where an application's streams went — until FxSound quits, so a bar never polls.
- **`fxsound --status --json` is a full document**: both lanes, the microphone's readouts and the
  echo canceller, every preset, both device lists, every band with its range, and the
  applications. It keeps every key 0.3.0 printed and carries every key the Windows build's
  `--status` writes, so a client written for either reads it.
- **`fxsound --self-test [--json]`** checks an installation without a display, a session or a
  sound server: the presets, the settings, both chains run offline with RNNoise, the desktop
  entry, the icons, the unit, the activation file, the man page and the metainfo. It is what CI
  runs after installing each package.
- **The command line reaches the microphone**: `--input`, `--next-input`, `--output=off` and
  `--input=off`, `--edit=output|input` and `--noise-suppression`.
- **The device priority list works.** Settings > Audio's list was drawn and never read. Every
  device now joins it and the engine follows it — a new device takes over only when it ranks
  above the one playing, and a lost one is replaced by the next one down — and the pickers, the
  tray and `--next-output` go in its order. The microphones have a list of their own. *Follow the
  system's default device* hands the choice back to the desktop, and a device that is no longer
  connected can be forgotten with the cross beside it, `--forget-device` or D-Bus `ForgetDevice`.
- **The effects column's second face.** The flip button turns it over, as in the original since
  1.2.12, to the band count, master gain, volume leveling, filter width, balance and Restore
  Defaults, which only the command line could reach before; on a microphone it shows the band
  count, the preset's makeup gain and the filter width.
- **Solo.** Ctrl+Alt+drag on an equalizer band, Alt+drag on Windows, sinks every other band while
  you listen and brings them back when you let go, without marking the preset changed.
- **Bulgarian**, new in the Windows build's 1.2.16, is the thirtieth language.
- **Every package built, installed and tested.** CI builds the Arch package, the Debian 13 and
  Ubuntu 24.04 `.deb`s, the Fedora RPM and the tarball in their own containers, installs each one
  and runs `fxsound --self-test`; a release carries all of them and one `SHA256SUMS`. Every package
  now installs the man page, the tray's status icons (which none shipped before), the AppStream
  metainfo, the D-Bus activation file, and the licences of the RNNoise code and the Noto faces
  built into the binary.

### Changed
- **FxSound's volume belongs to the device, and comes after the chain.** WirePlumber restored one
  volume for FxSound whatever it played to, so a level set for quiet laptop speakers came back at
  full on headphones. Each output now keeps its own, a device FxSound has no volume for is never
  given a louder one than it had, and a new device fades in over 30 ms. The volume is applied
  after the processing rather than in front of it, where Volume Leveling won back about 8 dB of a
  20 dB turn-down: turning FxSound down now turns it down.
- **Nothing runs while nothing plays.** The stream to the speakers sleeps while no application
  plays into FxSound, so the device and its real-time callback rest. A microphone is held open
  only while something records from `FxSound (Input)`, the calibration runs or the Pro view shows
  its meters, so a Bluetooth headset is not switched to call quality merely because its
  microphone is picked. The window repaints only when something has happened: idle on screen it
  takes 0.12 % of a core, where 0.3.0's ten repaints a second took 1–1.4 %.
- **Bluetooth headsets ride out their profile switches.** Opening a headset's microphone switches
  it to call mode, and its playback node vanishes and returns half a second later under the same
  name. FxSound now waits for it instead of jumping to the laptop speakers, and links it again when
  it returns. WirePlumber 0.5's Bluetooth microphone is recognised, and using one headset for both
  lanes says what that costs.
- **Mono outputs are accepted** — a Bluetooth headset in call mode, a mono USB headset — where
  0.3.0 refused them, as the Windows build does for a Windows driver's sake. A device that arrives
  in the same moment another leaves, a USB DAC swapped or a profile switch, is noticed as new.
- **A port that cannot be heard is not chosen**, such as an HDMI output with no monitor on it,
  while anything else of its direction can be; when nothing can be heard FxSound stays where it
  is, so a monitor that wakes simply plays again.
- **Suspend and resume.** Going to sleep mutes both lanes; waking clears their filters and runs
  the device rules again, giving Bluetooth a moment to come back, before the sound returns; should
  the wake-up never be announced, the sound returns after a minute awake. No sleep inhibitor is
  ever held.
- **Every change glides.** Master gain, balance, the effects, the equalizer's bands and filter
  width, the band count, the EQ switch, a preset change, a lane's mute and the microphone's makeup
  gain now move over about 20 ms instead of in one sample, so moving a slider no longer clicks or
  zips. What cannot glide dips: the power button, and on a microphone a preset change or the
  denoiser, the high-pass, the gate, the equalizer, the de-esser or the compressor switched in or
  out, fade the sound out over 10 ms, switch in silence and fade back in over 10 ms. The settled
  sound is the same.
- **The equalizer's curve is the response that plays**, filter width and neighbouring bands adding
  up included, where the Windows build joins the band values with straight lines.
- **The first and last bands turn both ways** on five and ten bands, reaching half a band past the
  old ends (46 Hz and 20 kHz on ten bands), where Windows lets the first only go up and the last
  only down.
- **The band count is yours.** Changing it carries the curve over, and a preset made for another
  count lands on yours, as in the original since 1.2.11 — but by frequency, where the original
  goes by position, so a boost stays where it was and a round trip between counts gives back the
  same curve. Changing the count is not an edit to the preset, and Restore Defaults keeps the
  count and the curve.
- **The window's controls.** Master gain and balance step by 1 dB, so every value the command line
  takes can be set by hand; an effect slider standing between two positions shows the decimal its
  preset stores, and Shift steps one stored value at a time; a press on a thumb or a band's knob
  moves nothing until the pointer does, and one wheel notch is one step; a right-click puts any
  slider back to its default, an effect to 0. The Volume Leveling readout shows a bare number, 1.5
  rather than 1.5 dB, since the setting is a strength, not decibels. With the power off the values
  stay on screen, the sliders greyed as the equalizer is, and a preset can still be picked, from
  the window and the tray. Every title-bar button and the flip say on hover what they do.
- **Presets are harder to lose.** Delete Preset asks first and moves the file, with its unsaved
  edits, to the desktop's trash; unsaved edits are written to disk a minute after the first one;
  Save New Preset also copies a preset with no changes; a rename can change only the letter case.
  Reset presets is offered only while some preset has unsaved changes, asks first, and says what it
  did — unsaved changes discarded, every saved preset kept — where it claimed to have restored the
  factory defaults.
- **Names that travel.** A new preset's name is cut to the 126 bytes a Windows FxSound reads a name
  in (63 Cyrillic letters), a line break or a tab in it becomes a space, and an import of two
  files that come out under one name keeps the first and says it skipped the second.
- **The command line leaves the window alone.** Only `--show`, `--view` and a bare `fxsound` bring
  it up, so a keybind's `--preset=Gaming` changes the preset without pulling FxSound over the
  game, and a line of such options that starts FxSound starts it in the tray. Every option now
  also works when it is what starts FxSound, where band lists, effects and preset commands were
  dropped.
- **The command line says what it will not do.** Two preset options on one line, a band the
  equalizer does not have, a band frequency outside its band's range, an unknown preset and an
  unknown language are errors that say so, where they did nothing, or half of it, without a word.
- **The tray** lists outputs under *Playback Device Select* and microphones under *Recording
  Device Select*, shortens a long device name in its middle rather than cutting it at 30
  characters, and draws its own icon for each state.
- **Languages** are listed by their own names, in alphabetical order after English, and
  `--language` also takes the ISO codes Windows spells its own way (`uk`, `bs`, `nb`, `nn`) and
  locales such as `ru_RU.UTF-8`.
- **Where the window goes.** FxSound quit with its window in the tray starts in the tray again, as
  on Windows. On a desktop with no tray icon the minimise button minimises rather than hides, and
  a start that would have hidden the window brings it up minimised. An FxSound started with no
  display to open a window on — by a D-Bus call through the systemd user unit, say — stays in the
  tray and says so when its window is asked for.
- **Settings files.** One that does not load is moved aside to `settings.toml.bad`, never over an
  earlier one, before the defaults are used; a settings file, `apps.toml` or a preset that is a
  symbolic link, as GNU Stow or chezmoi leave them, stays a link when it is saved, and a link to a
  read-only file is neither written through nor replaced, so every save of that file fails.
- **The light theme** keeps the greyed equalizer and visualizer, the borders, the dividers and the
  Settings tab captions visible, where they came out nearly white on white.
- **The suggested keybindings use Super+Alt**, in the man page, the README and the Hyprland
  example: a compositor binding takes its keys from every application, and the Windows build's
  Ctrl+Shift+Q is how Chromium quits.
- The package descriptions — the `.deb`'s, and the AppStream metainfo GNOME Software and Discover
  show — no longer say a preset voiced on Windows sounds the same here, since this release fixes
  defects of the original DSP, and the metainfo now describes the release itself.
- The Fedora package's `License` tag is taken again from this release's dependency tree: it no
  longer names BlueOak-1.0.0, which only a crate the vendored RNNoise has stopped linking carried.
- The README describes both lanes, per-application presets, the D-Bus interface and a Waybar module
  built on `fxsound --watch --json`, and says that this port is not affiliated with FxSound LLC;
  the manual page's examples name a preset that ships.

### Fixed
- **Bass comes out clean.** The limiter behind Dynamic Boost overshot on the rise of a bass note
  and breathed within each of its cycles; it now holds its gain for 20 ms before letting go. A bass
  tone driven into it had up to 18.6 % distortion with the shipped presets, and has at most
  0.016 % now.
- **Volume Leveling no longer clips the bass.** It took its safety peak from a high-passed side
  chain, so it lifted deep bass and equalizer bass boosts past full scale into a hard clip. It
  also runs at one speed whatever buffer size and rate PipeWire uses, where it sped up when a game
  or a voice chat shrank the buffer; it lifts the subwoofer with the other speakers on 5.1 and
  7.1; and it dips over a couple of milliseconds just before a loud hit instead of stepping down
  10 ms early.
- **Dynamic Boost at 0 lifts nothing.** Loud music was pushed half a decibel into the limiter at 0,
  which is where Flat and the lowest values Windows stores sit. Its loudness is also judged from
  both front channels instead of the left one alone.
- **The limiter turns both sides down together**, so a loud peak on one side no longer drags the
  stereo image towards the other by up to 5.6 dB; on 5.1 and 7.1 each side is linked, and the
  centre and the subwoofer keep their own. The microphone's limiter is linked the same way.
- **Effects and bands start clean.** Switching Ambience, Bass or Fidelity off and on no longer
  replays up to 150 ms of old music or clicks, and a band back from 0 dB, the EQ switch and the
  power button no longer thump with bass heard before they were switched off.
- **The power button and the microphone's presets no longer click.** Under a steady tone, the power
  button stepped the speakers by up to -5 dBFS (high-passed at 2 kHz) for an application that
  stays on FxSound's sink, and a voice preset change or the voice equalizer's switch stepped the
  recording by up to -21 dBFS; now the power button stays at or under -48 dBFS and the voice
  presets under -70 dBFS. An application moved back onto FxSound when the power comes on, in the
  middle of a note, fades in instead of starting at full level after the ring's priming (it
  popped at -5 dBFS), as does any sound after a quarter of a second of digital silence. What is
  left on a power toggle, about -19 to -26 dBFS, is WirePlumber moving the stream between FxSound
  and the device, the same as moving it between two devices without FxSound.
- **Following the system's default device no longer loses FxSound.** Once a device had been
  picked in FxSound, a device picked in the desktop's sound settings moved nothing, and the
  default stayed on it, so every application played past FxSound; now the lane follows the
  desktop's pick and FxSound takes the default back, also when the desktop picks the very device
  FxSound is playing to.
- **An application recording from the default source records FxSound again after the power
  button.** Now and then WirePlumber moved such a recorder from the microphone back onto FxSound
  (Input) and linked it to nothing, and it recorded nothing until the default changed again (3 to
  6 of 9 quick toggles in the live check); FxSound now finds a stream it took over that was left
  unlinked and has WirePlumber link it again within about a second.
- The README and the manual page told a status bar to read a property with `busctl --user
  --auto-start=no get-property`, which starts FxSound anyway (systemd 262's `busctl` honours the
  flag only for `call`); they now read it with `busctl --user --auto-start=no call …
  org.freedesktop.DBus.Properties Get ss org.fxsound.FxSound Power`.
- The terminal and the journal no longer get six lines of the D-Bus library's authentication
  handshake, raw bytes and all, for every desktop notification and every bus connection:
  FxSound logs the bus library's warnings and errors only, unless `RUST_LOG` says otherwise.
- An equalizer band set below 20 Hz, which only a hand-made or imported preset can do, raised the
  whole spectrum by its gain; it now lifts only the deep bass around it.
- The balance on 5.1 and 7.1 turned down only the front speaker of a side; it turns down the whole
  side now, and leaves the centre and the subwoofer alone.
- Ambience's positions 1 to 3 were all but silent; its ten positions now run over the values that
  can be heard.
- The 20-band equalizer's bands were paired up on the Windows ladder, so a broad boost rippled by
  3.9 dB; spaced every half octave they ripple by 1.9 dB.
- The denoiser's delay was reported to PipeWire as 10 ms and is 20 ms, so a recording application
  drifted out of lip sync by the difference; and every preset change rebuilt eight of its network
  states on the audio thread.
- The notice in the window that sixteen code paths had been writing since 0.2.0 is drawn at last.
- The tray had no icon on any packaged install.
- Six microphone strings from 0.3.0 were English in every language. A test now fails if any
  language misses a string.
- A voice preset's master gain or filter width could leak onto the speakers, and saving on a
  microphone could write a `.fac` named after a voice preset.
- A settings file with one error in it was reset to the defaults, and one unreadable voice preset
  hid all of them.
- Picking a preset that does not exist, from the command line, succeeded without doing anything.
- Export wrote a preset's unsaved edits instead of the preset as saved.
- A device picked before its channel count arrived stayed at the wrong width, and a profile switch
  that changed the channel count was not followed.
- A preset 0.3.0 saved under a name with `:`, `?` or another character a file name cannot hold is
  saved to its own file again, and the unsaved edits 0.3.0 kept for it come back.
- A preset named with a line break in it, a pasted `--save_preset` say, was saved as a file that
  could not be read back, picked, renamed or deleted.
- A Windows preset of version 7 or older keeps its equalizer when it is saved again, and a version
  1 file is no longer written back unreadable.
- Changing the number of bands on the speakers marked the preset as changed, and saved the fitted
  curve over it on the way out.
- Enter in the Rename Preset box renamed whichever preset was selected by then.
- `max_user_presets` outside 10–120 was read as 120; it is now held to 10–1000.
- The lines the Windows translation tables leave unclosed, or write without a space after the `=`,
  show translated, as they do on Windows.
- A muted microphone kept the voice chain doing slow arithmetic for as long as it stayed muted.
- After every preset change Dynamic Boost played its full boost for about a second and a half,
  having forgotten how loud the music was; it now keeps listening across the change.
- The lit thumbs of the sliders and of the equalizer were drawn a quarter of their width, and a
  slider's coloured fill ran 8 px past the end of its track.
- A preset name too long for its box lost the mark that says it has unsaved changes; it is
  shortened with an ellipsis now and keeps the mark.
- The question and notice boxes cut their text after one line; they wrap it, so a long translation
  or preset name is shown whole.
- Some of the Settings tabs' captions ran into the divider or were cut off; they now fit whole
  short of it in every language, set a little smaller where a translation is long.
- Export and Import Presets were greyed out while the selected preset had unsaved changes.
- The changelog in Settings > Help showed an empty box for a symbol the window's fonts do not
  have; a test now holds it to the characters they do.
- `packaging/build-tarball.sh` packed whatever binary was left in `target/release`; it now builds
  the tree it sits in first and refuses a binary of another version.
- In the Lite view the hamburger menu lost Export Presets, Import Presets and Theme below the
  window's 189-point edge and was pushed up over the title bar, and the preset and device lists
  showed three rows with a cut edge. The menu now hangs under the hamburger and scrolls inside
  the window, and a list scrolls between the title bar and the window's bottom edge, over its own
  box when there is no room under it. The window does not grow for them: a compositor that draws
  round or behind a window draws round the grown, transparent part too (niri filled it with its
  focus ring's colour), and Hyprland does not let a floating window grow at all.
- On Hyprland the floating window stayed at the size it was started at: flipping between Pro and
  Lite or opening Settings from the Lite view left the view cut off. The window now gives the
  compositor each view's size as its smallest and largest, which Hyprland holds a floating window
  to.
- A long preset list in the Pro view was pushed up over the title bar; it now hangs under its box
  and scrolls there.
- The hamburger menu kept a fixed width, and a long item ran over its edge — German
  "Voreinstellungs-Änderung verwerfen", English "Overwrite Existing Preset - My Preset"; it is as
  wide as its longest item now, and the name field under Save New Preset and Rename Preset shows
  its hint whole.
- A dialog button's label one word too long for it was broken over two lines and out of the button
  (German "Exportieren", Russian "Сохранить" under Export Presets); it is set smaller instead, and
  so are the Import and Export dialogs' headings and Settings > General's hotkey names where a
  translation is long, rather than cut off with an ellipsis.
- The user's presets were sorted in among the unnumbered factory presets, so the list's rule
  between factory and user presets fell among factory ones and the window's list read otherwise
  than the tray's; the user's presets now follow all the factory ones, by name.
- The folder picker for Import Presets was titled in English whatever the window's language.
- A tiling compositor's column narrower than the window (niri's default half-screen one) cut the
  Pro view off on both sides; the window is scaled down to fit instead, and a size asked for while
  scaled no longer comes out scaled too.
- The German and Russian texts FxSound added called a preset "Preset" or "пресет" where the rest
  of the window says "Voreinstellung" or "шаблон", and German Settings > Apps cut "Preset von
  FxSound" to "Preset von FxSo…"; it reads "Wie FxSound".
- Italian and Arabic Settings > Apps cut "FxSound's preset" in the same way ("Preset di
  FxSou…"); they read "Come FxSound" and "قالب FxSound".
- With the name field of Save New Preset or Rename Preset open, the hamburger menu in the Pro
  view kept the height it had without it, and Light was lost below Dark; it now grows to hold the
  field.
- The note under the effect sliders while a microphone is selected ran past the edge of their
  panel in Italian ("Senza effetto su un microfono"); it is set a little smaller where a
  translation would run past it.
- The Russian question before exporting over several existing preset files read "2 файлов
  шаблонов…", the number before a noun it does not agree with; it now gives the number at the end.
- A binary installed under `/usr/local`, as the tarball installs it, took the factory and voice
  presets a distribution package had left under `/usr` before its own, so beside an older package
  it listed that release's copies, older genre voicings among them; each binary now looks under
  its own prefix first.

### Removed
- The tray's *Always On Top* item, which winit cannot honour on Wayland.
- The Windows keys nothing reads — the hotkey chords, the window position, always-on-top and the
  update check — are no longer written to `settings.toml`. An older file that has them still loads.

### Upgrading from 0.3.0
- **A microphone left on comes back beside the speakers.** Settings, presets and unsaved edits
  carry over. If 0.3.0 was left on a microphone, the microphone comes back and the speakers now
  play through FxSound beside it; pick Off in the output picker, or run `fxsound --output=off`, to
  keep FxSound on the microphone alone.
- **Power off takes FxSound out of the path.** 0.3.0 kept every application on FxSound's nodes
  with the effects off. Now, as in the Windows build since 1.2.6, power off hands the session's
  default output and input back to the real devices, applications with a preset of their own
  included, and power on takes them again. An FxSound started with its power off leaves them there.
- **Neither the power button nor the EQ switch moves the level any more.** Master gain and balance
  play whatever either says: switching FxSound off no longer re-centres the balance, and switching
  the equalizer off, which now takes Volume Leveling with it as on Windows, keeps the master gain
  and the balance, which Windows drops.
- **Picking the preset that is already selected changes nothing.** `--preset` with the current
  preset's name, D-Bus `SetPreset` and a lone `--next-preset` on a one-preset list used to reload
  it from disk and throw away the edits since the last autosave; they keep them now, as picking it
  again in the window does. `--undo_preset` reads a preset back from its file.
- **The command line is stricter.** The errors listed under Changed exit with status 2 or 1 where
  0.3.0 carried on; `--next-output` and `--next-input` with no FxSound running are refused rather
  than starting it; and options that set something no longer show the window, so add `--show`
  where a script wants it.
- **The 20-band equalizer's centres moved** to every half octave from 20 Hz to 16 kHz, and its
  captions read 28 Hz, 57 Hz, 1.4 kHz and so on. A 20-band curve on the Windows frequencies, from
  a Windows preset or saved by 0.3.0, is moved onto the new bands band for band as it loads, and
  exported back on the Windows frequencies. The 10- and 31-band equalizers are unchanged.
- **Ambience reads lower on the factory presets.** Its positions now cover only the values that
  can be heard, so a factory preset sounds exactly as it did but shows lower on the slider: a
  stored 64 reads 3.6 instead of 5.
- **An exported `.fac` puts the end bands back in the Windows ranges.** A first band below 62.5 Hz
  or a last band above 16 kHz, which the wheels can now reach, is written at 62.5 Hz or 16 kHz, so
  Windows FxSound reads the file as it always did. That also moves the first band of the port's own
  Trap (50 Hz) and Competitive FPS (60 Hz) in their exports.
- **The sound differs from 0.3.0 and from the Windows build, on purpose.** This release fixes the
  original DSP's defects rather than copying them, so a preset plays close to, not exactly as, it
  did: bass driven into the limiter comes out clean, and loud, finished masters play 1.4–3.5 LU
  quieter because the limiter no longer squeezes the bass into loudness; Dynamic Boost at 0 lifts
  nothing and hears both front channels; the stereo image stays put under limiting; Volume
  Leveling keeps deep bass clean, reacts at one speed and lifts the subwoofer too; a band count
  change keeps the curve by frequency; the balance turns down a whole side on surround; and no
  control clicks when it moves. No shipped preset was re-voiced, and each genre preset still ranks
  where it did for its genre.
- **The volume on `FxSound (Output)` means what it says.** It is applied after the chain now, so
  a turned-down FxSound with Volume Leveling on plays quieter than 0.3.0 did, by up to 8 dB, and it
  is remembered per output device.
- **Delete Preset asks, and moves the preset to the trash** rather than deleting it for good at one
  click; a file manager restores it, unsaved edits and all.
- **FxSound quit with its window in the tray starts in the tray**, from the launcher too; launching
  it again, or `fxsound --show`, brings the window.
- **A stream parked by hand on one of FxSound's per-application nodes**, named like
  `FxSound (Output) · Gaming`, is moved back to where its application's rule puts it; choose an
  application's preset in Settings > Applications instead. A stream moved by hand anywhere else
  stays where you put it, and so does a recorder pointed at such a node's monitor.
- **A settings file linked to a read-only file is no longer saved.** 0.3.0 saved a settings file,
  `apps.toml` or a preset that is a link by replacing the link with a plain file. A link to a
  read-only file, as home-manager's default links into the Nix store are, is now left alone and
  every save of that file fails; link it with home-manager's `mkOutOfStoreSymlink` to let FxSound
  save it.
- **Keybindings.** If you copied the Windows build's chords from an older example, Ctrl+Shift+Q
  among them, move them: see the man page's EXAMPLES.

## [0.3.0] — 2026-09-21

### Added
- **A microphone chain of its own.** A voice runs through denoising, a high-pass, a downward
  expander, the ten-band equalizer, a de-esser, a compressor, makeup gain and a look-ahead
  limiter — a separate path from the music chain, which exists to make music sound bigger and is
  the opposite of what a voice wants. Picking a microphone now runs the voice chain rather than
  applying reverberation and a bass lift to someone's speech.
- **RNNoise**, in front of everything that measures a level. Off unless a preset asks for it. On
  a desk microphone's own floor — hum under hiss — it removes about 44 dB.
- **Voice presets**, in TOML rather than `.fac`: the `.fac` format is a byte-for-byte contract
  with the Windows build and has nowhere to put a gate threshold. The preset picker shows the
  voice set in front of a microphone and the `.fac` set in front of a speaker, and never mixes
  them.
- Presets are remembered **per direction**: a preset chosen for a microphone no longer follows you
  back to your speakers.
- The window says what a microphone does and does not use: the five effect sliders are drawn
  disabled with the reason underneath, the voice chain's stages read out along the bottom of the
  panel, and an equalizer band the device cannot carry is struck through rather than left looking
  live.
- Continuous integration, a release workflow that refuses a tag that disagrees with `Cargo.toml`,
  and two AUR packages.

### Changed
- Every stage now follows the channel layout the device reports instead of assuming the first two
  channels are the front pair. A node's PipeWire registry entry carries no channel count at all —
  it arrives once the node is bound — so before this every device ran as stereo.
- The DSP's latency is published to PipeWire and **kept up to date**: switching the denoiser on
  adds ten milliseconds, and a recording application that was told the old figure drifts out of
  lip sync by exactly that much.
- A capture stream is asked for at 48 kHz whatever the microphone runs at, so that a voice preset
  means one thing on every device.
- Settings and presets are written durably: a temporary file, an fsync, a rename. An interrupted
  save can no longer truncate what was there.
- Twelve of the shipped genre presets were revoiced away from each other, and three new ones
  added: Flat, Laptop & Small Speakers and Competitive FPS.

### Fixed
- Eight presets stored an Ambience value the engine silently ignored — the slider had been moved,
  the file recorded it, and nothing happened.
- Three presets had two equalizer bands one hertz apart, so the pair added and the preset was
  6 dB louder there than it read.
- Nothing non-finite can reach the filters any more, from a device, a corrupt settings file or a
  hand-edited preset. A stage that blows up on its own is reset rather than shipped.
- The audio ring's cushion is sized from the block the consumer actually takes rather than from a
  hard-coded 512, which stops a persistently short consumer clicking every cycle.

## [0.2.0] — 2026-09-14

### Added
- **Input mode.** The device list is split into *Output* and *Input* sections. Picking a
  microphone turns FxSound into a virtual source (`FxSound (Input)`) that becomes the session's
  default microphone; picking an output again reverses it.
- FxSound now becomes the session default output automatically when an output device is chosen,
  and hands the previous default back on exit, on a mode switch, and on `SIGTERM`/`SIGINT`.
- System-visible node names follow the desktop locale (`FxSound (Вывод)` / `FxSound (Ввод)` on a
  Russian desktop).
- The user interface is translated: the Windows build's 28 translation tables are embedded, the
  language follows the desktop session by default, and it can be picked explicitly in Settings.
- The hamburger menu carries the original's items: Save New Preset and Rename Preset with their
  inline name editors, Export and Import Presets, and a Dark/Light theme switch.
- Desktop notifications for preset, output and power changes, honouring *Hide notifications*.
- This changelog, readable from Settings > Help.

### Changed
- Closing the window (the close button, the minimise button, or the compositor's close request
  such as Hyprland's `killactive`) hides FxSound to the system tray; audio keeps processing. Only
  the tray's Exit or `fxsound --quit` quit.
- The window no longer stops answering the compositor when it sits on a hidden workspace: the
  GL surface runs without vsync and the UI paces its own repaints.
- The first effect is labelled *Clarity* and the third *Surround Sound*, as in the current
  Windows build.
- The window scales itself to fit when the compositor makes it larger than its design size
  (fullscreen or maximised), instead of drawing into a corner.
- *Launch on system startup* reflects the real state of the XDG autostart entry.

### Removed
- The Donate button, the Donate menu and tray items, *Check for updates*, *Download Bonus
  Presets*, *Help center* and the inert *Automatic updates* toggle. This fork does not solicit
  money for the upstream developers and never contacts the network.

### Fixed
- Presets were listed by file name (`1`, `2`, …) instead of by the name inside the file.
- No output device was ever accepted: a device whose channel count was not yet known was
  treated as mono and refused, so the engine never started.
- Switching output devices failed with "PipeWire disconnected" because the DSP state was not
  handed back before the new nodes were built.
- The default-device hand-back on exit was queued after the event loop had stopped and never
  reached the server, leaving `default.configured.audio.sink` pointing at a node that no longer
  existed.
- `fxsound --quit` and `fxsound --status` with no running instance started a new one.
- The tray menu went stale: *Always On Top*, modified-preset markers and device names were not
  refreshed.
- The hamburger menu closed on the frame it opened.

## [0.1.0] — 2026-09-13

### Added
- Initial Rust/egui port for Linux: native Wayland window at the original's geometry, PipeWire
  virtual sink with the DSP ported line by line from the original C, factory and bonus presets,
  the Settings pane, a StatusNotifierItem tray, a single-instance control socket for compositor
  keybinds, and an Arch package.
