# Changelog

All notable changes to the FxSound Linux port. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

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
- Closing the window (✕, the minimise button, or the compositor's close request such as
  Hyprland's `killactive`) hides FxSound to the system tray; audio keeps processing. Only the
  tray's Exit or `fxsound --quit` quit.
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
