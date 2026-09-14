# 07 — Process lifecycle, command line, system tray

Reverse-engineering spec for the Rust/egui Linux port of FxSound.
Subsystem: **process lifecycle, CLI, system tray, notifications, autostart.**

---

## 0. Scope and provenance

Everything in this document was read from the source tree at
`/home/blackixxce/Загрузки/fxsound-app-main`. Every concrete number, string and
identifier below carries a `path:line` citation. Paths are relative to that root.

Files read in full:

| File | Lines | What it owns |
|---|---|---|
| `fxsound/Source/Main.cpp` | 331 | `JUCEApplication` subclass: init, shutdown, crash filter, single-instance hook |
| `fxsound/Source/GUI/FxSystemTrayView.h` | 68 | Tray menu ids, tray GUID decl, callback message id |
| `fxsound/Source/GUI/FxSystemTrayView.cpp` | 477 | Tray icon lifecycle, menu tree, balloon/toast, window subclass |
| `docs/COMMAND_LINE_OPTIONS.md` | 128 | Normative CLI documentation |

Supporting files read for the parts this subsystem depends on:

- `fxsound/Source/GUI/FxController.h` (298 lines), `FxController.cpp` (2914 lines) — `initConfig`, `applyConfig`, `printStatus`, `init`, `exit`, show/hide window, autostart registry, hotkeys, session/power events.
- `fxsound/Source/GUI/FxNotification.h` / `.cpp` — the in-app toast geometry.
- `fxsound/Source/GUI/FxMainWindow.h` / `.cpp` — show/hide/close semantics, hamburger menu (tray-menu parity).
- `fxsound/Source/GUI/FxModel.h`, `FxTheme.h`, `FxTheme.cpp` — event enum, message slot, theme mode, colour table.
- `fxsound/Source/Utils/Settings/Settings.h` / `.cpp`, `fxsound/Source/Utils/SysInfo/SysInfo.cpp`.
- `fxsound/Project/*` and `fxsound/ProjectARM/*` (vcxproj, .rc, .sln, .settings, .ico), `fxsound/FxSound.jucer`, `fxsound/FxSoundARM.jucer`, `fxsound/JuceLibraryCode/JuceHeader.h`.
- `Installer/fxsound.aip` (Advanced Installer project) — shortcuts, install dirs, product codes.
- `fxmcp/internal/fxsound/{config,process,running,status,locate}.go` — an in-tree second source that documents the IPC contract from the *client* side.

**Not in this tree:** the JUCE 6.1.6 module sources (`C:\JUCE\modules`, referenced at
`fxsound/FxSound.jucer` MODULEPATHS). Where JUCE internals matter (single-instance
transport, `ArgumentList` tokenising) this document says so explicitly and falls back
to the two in-tree sources that describe the observable contract
(`docs/COMMAND_LINE_OPTIONS.md` and `fxmcp/internal/fxsound/process.go`).

**Also not in this tree:** `Resources/Strings/*.txt` — the directory exists but is
empty in this checkout (`Resources/` contains nothing). The `.jucer` lists 30
translation files (`fxsound/FxSound.jucer`, `Strings` GROUP). Therefore the only
authoritative strings are the English `TRANS("…")` keys embedded in the C++, which
are quoted verbatim throughout this document.

---

## 1. Application identity and build configuration

### 1.1 Names, versions, ids

| Fact | Value | Source |
|---|---|---|
| Project name | `FxSound` | `fxsound/JuceLibraryCode/JuceHeader.h:46` |
| Company | `FxSound LLC` | `fxsound/JuceLibraryCode/JuceHeader.h:47` |
| Company website | `https://www.fxsound.com` | `fxsound/FxSound.jucer:5` |
| Copyright | `Copyright (C) 2026 FxSound LLC` | `fxsound/FxSound.jucer:5`, `fxsound/Project/resources.rc:19` |
| `.jucer` project version | `1.2.15.0` | `fxsound/FxSound.jucer:3`, `fxsound/FxSoundARM.jucer:3` |
| RC `FILEVERSION` / `ProductVersion` | `1.2.15.0` | `fxsound/Project/resources.rc:12,21,23` |
| `JUCE_APP_VERSION` define | `1.2.15.0`, hex `0x1020f00` | `fxsound/Project/FxSound_App.vcxproj:182` |
| **`ProjectInfo::versionString` (what the app actually reports)** | **`1.2.14.0`**, `versionNumber = 0x1020e00` | `fxsound/JuceLibraryCode/JuceHeader.h:48,49` |
| Installer `ProductVersion` | `1.2.15.0` | `Installer/fxsound.aip:32` |
| Installer `ProductCode` | `{1A5BFC59-A426-4F0E-B8E8-2CB0304AC7B9}` | `Installer/fxsound.aip:28` |
| Installer `UpgradeCode` | `{1CA2081B-0D5A-41DF-86E8-2788204CE340}` | `Installer/fxsound.aip:36` |
| Tray icon GUID | `{A8E96325-5269-443C-A0D8-0D02562FE553}` | `fxsound/Source/GUI/FxSystemTrayView.cpp:23-25` |
| Hidden hotkey window title | `FxSoundHotkeys` | `fxsound/Source/GUI/FxController.cpp:126` |
| Hidden window class prefix | `FXSOUND_` + hex of `Time::getHighResolutionTicks()` | `fxsound/Source/GUI/FxController.h:186-187` |
| Target exe name | `FxSound` → `FxSound.exe` | `fxsound/Project/FxSound_App.vcxproj:64-65,71-72`; `TargetExt .exe` at `:57` |
| App vcxproj GUID | `{7A8BC8D1-8FA6-3204-17E8-DA0BF486BB4E}` | `fxsound/Project/FxSound.sln:6` |

> ⚠️ **Version inconsistency, carry it forward deliberately.** `ProjectInfo::versionString`
> is `1.2.14.0` (`JuceHeader.h:48`) while every other source of truth says `1.2.15.0`.
> `ProjectInfo::versionString` is what `getApplicationVersion()` returns
> (`Main.cpp:45`), what is logged at startup (`FxController.cpp:155`), what is written
> to `status.json` as `"version"` (`FxController.cpp:641`), and what drives the
> "new version" first-run branch (`FxController.cpp:713-723`). The Rust port must have
> **one** version constant (`env!("CARGO_PKG_VERSION")`) used for all four purposes.

### 1.2 Build-configuration facts that matter to a port

| Fact | Value | Source |
|---|---|---|
| Toolset / std | MSVC `v143`, `stdcpp17` | `fxsound/Project/FxSound_App.vcxproj:29,101` |
| Subsystem | `Windows` (GUI, no console) | `fxsound/Project/FxSound_App.vcxproj:112,204` |
| Extra libs | `crypt32.lib;Wtsapi32.lib` | `fxsound/Project/FxSound_App.vcxproj:114,209`; `fxsound/FxSound.jucer` VS2022 `externalLibraries` |
| Explicit defines (Release x64) | `_CRT_SECURE_NO_WARNINGS;WIN32;_WINDOWS;NDEBUG;UNICODE;JUCE_MODAL_LOOPS_PERMITTED;JUCER_VS2022_50C8E2F9=1;JUCE_APP_VERSION=1.2.15.0;JUCE_APP_VERSION_HEX=0x1020f00` + all `JucePlugin_Build_*=0` | `fxsound/Project/FxSound_App.vcxproj:235` |
| ARM64 variant define token | `JUCER_VS2022_1660CA3=1`, adds `JucePlugin_Build_LV2=0` | `fxsound/ProjectARM/FxSound_App.vcxproj:126` |
| Extra compiler flags | `/bigobj /Qpar` | `fxsound/FxSound.jucer` VS2022 `extraCompilerFlags` |
| Output layout | `$(SolutionDir)$(Platform)\$(Configuration)\App\` | `fxsound/Project/FxSound_App.vcxproj:58-61` |
| Release post-build | copies `FxSound.exe` + `.pdb` into `bin\$(PlatformTarget)\` | `fxsound/FxSound.jucer` Release CONFIGURATION `postbuildCommand` |
| Platforms in `Project/` | `Debug|Win32`, `Debug|x64`, `Release|Win32`(→ builds x64), `Release|x64` | `fxsound/Project/FxSound.sln:14-27` |
| Platforms in `ProjectARM/` | `Debug|ARM64`, `Release|ARM64` only | `fxsound/ProjectARM/FxSound.sln:14-15` |
| Sibling projects in the solution | `audiopassthru` `{7685E345-…}`, `DfxDsp` `{F72F101C-…}` | `fxsound/Project/FxSound.sln:8,10` |
| Projucer export hook | appends `Project\status_icons.rc` onto `Project\resources.rc` after every export | `fxsound/FxSound.jucer:6` (`postExportShellCommandWin`) |

`JUCE_MODAL_LOOPS_PERMITTED` is load-bearing: the tray's **Settings** item runs a
*blocking modal loop* (`FxSystemTrayView.cpp:261-265`, `settings_dialog.runModalLoop()`).
egui is immediate-mode and has no modal loop; see §5.6.

Two graphics-driver hints are exported from the binary, both set to **0** to *avoid*
the discrete GPU on hybrid laptops (`Main.cpp:31-35`):

```
__declspec(dllexport) DWORD NvOptimusEnablement = 0x00000000;
__declspec(dllexport) int   AmdPowerXpressRequestHighPerformance = 0;
```

**Linux equivalent:** there is no symbol-based equivalent. On a hybrid-GPU laptop,
wgpu/vulkan will pick whatever the `VK_ICD` / DRI PRIME environment says. Match the
intent (stay on the iGPU, this is a tray utility, not a game) by:
- preferring `wgpu::PowerPreference::LowPower` when creating the adapter, and
- shipping the `.desktop` file **without** `PrefersNonDefaultGPU=true` (the freedesktop
  key that switchers like `switcheroo-control` read), and without `X-KDE-RunOnDiscreteGpu`.

### 1.3 Icon resources

`fxsound/Project/resources.rc:35-40` declares six icon resources. `status_icons.rc`
is appended by the export hook, which is why `IDI_LOGO_*` appear twice in the ARM copy
(`fxsound/ProjectARM/resources_arm.rc` repeats the trio seven times — a cosmetic export
artefact, not meaningful).

| Resource name | File | ICO contents (parsed) | Opaque pixel colours (32×32 frame) |
|---|---|---|---|
| `IDI_ICON1`, `IDI_ICON2` | `icon.ico` | 16,32,48 BMP + 256 PNG, 32bpp | `#000000` bg, `#ffffff` bars |
| `IDI_LOGO_WHITE` | `white_logo.ico` | 16,24,32,48 BMP + 256 PNG | `#000000` bg, `#ffffff` bars |
| `IDI_LOGO_RED` | `red_logo.ico` | 16,24,32,48 BMP + 256 PNG | `#000000` bg, `#ea3564` bars |
| `IDI_LOGO_GRAY` | `gray_logo.ico` | 16,24,32,48 BMP + 256 PNG | `#000000` bg, `#6f6f6f` bars |
| `IDI_LOGO_BLUE` | `blue_logo.ico` | **only 256×256 PNG** | `#000000` bg, `#23b6eb` bars |

(ICO directories parsed from the files in `fxsound/Project/`; 976 of 1024 pixels in each
32×32 frame are fully opaque, i.e. **these tray icons have an opaque black square
background** — they are not transparent glyphs.)

Brand colours, from the vector originals (these are the authoritative hexes):

| Asset | Fill | Source |
|---|---|---|
| `Images/logo-red.svg` | `#e63462` | `fxsound/Images/logo-red.svg:1` |
| `Images/logo-blue.svg` | `#23B6EB` | `fxsound/Images/logo-blue.svg:1` |
| `Images/logo-white.svg` | `#fff` | `fxsound/Images/logo-white.svg:1` |
| `Images/logo-black.svg` | `#000` | `fxsound/Images/logo-black.svg:1` |
| `Images/FxSound White Bars.svg` (icon glyph) | `#fff`, viewBox `0 0 299.83 219.26`, five rounded rects `rx=2.83` | `fxsound/Images/FxSound White Bars.svg:1` |

The 5-bar glyph geometry (use this to regenerate Linux icons at any size), from
`FxSound White Bars.svg:1`:

```
bar   x        y        w       h        rx
1     0        131.51   43.22   87.75    2.83
2     64.16    60.96    43.22   158.30   2.83
3     128.31   0        43.22   219.26   2.83     <- tallest, centre
4     192.46   60.97    43.22   158.29   2.83
5     256.61   131.52   43.22   87.75    2.83
viewBox = 0 0 299.83 219.26
```

### 1.4 What the Windows installer registers (and what it does *not*)

From `Installer/fxsound.aip`:

| Thing | Value | Line |
|---|---|---|
| Install dir | `[ProgramFilesFolder]FxSound LLC\FxSound` | `:306` |
| Start-menu dir | `[ProgramMenuFolder]FxSound` | `:307` |
| Start-menu shortcut | `FxSound` → `FxSound.exe`, **no arguments** | `:458` |
| Desktop shortcut | `FxSound` → `FxSound.exe`, no arguments | `:459` |
| **Startup-folder shortcut** | `FxSound` in `StartupFolder` → `FxSound.exe`, **no arguments** | `:457` |
| Updater shortcut | `Check for FxSound updates` → `updater.exe /checknow` | `:456` |
| Machine-wide defaults file | `Resources\FxSound.settings` → `%ProgramData%\FxSound\FxSound.settings` | `:81,117`; dir at `:55` |
| Factory presets | `Factsoft\*.fac` (1.fac…12.fac, Default.fac) under the install dir | `:54,75,142-165` |
| Elevation | `UACExecutionLevel="2"` (requires admin) | `:190` |
| **File associations** | **none** — the project has no `Extension`/`ProgId`/`Verb` rows at all (grep: 0 matches) | — |

So: `.fac` presets are **not** registered as a file type, and there is no URI scheme.
Preset import/export is done entirely through in-app file dialogs
(`FxPresetImportDialog` / `FxPresetExportDialog`, `FxMainWindow.cpp` hamburger menu).

Note the installer ships a Startup-folder shortcut *with no arguments*, i.e. the
out-of-the-box autostart launches FxSound **normally**, not minimised. Whether the
window appears is then decided by the persisted `run_minimized` setting (§7.1).

### 1.5 Linux identity mapping (normative for the port)

| Concept | Windows | **Linux value to use** |
|---|---|---|
| App id | exe name `FxSound.exe` | **`com.fxsound.FxSound`** |
| Binary | `FxSound.exe` | `/usr/bin/fxsound` |
| Desktop entry | Start-menu `.lnk` | `/usr/share/applications/com.fxsound.FxSound.desktop` |
| Wayland `app_id` (xdg-toplevel) | n/a (HWND class) | `com.fxsound.FxSound` — set via `egui::ViewportBuilder::with_app_id("com.fxsound.FxSound")` |
| Icon theme name | `IDI_ICON1` | `com.fxsound.FxSound` in `hicolor/{16,24,32,48,64,128,256}x*/apps/` + `scalable/apps/com.fxsound.FxSound.svg` |
| Tray icon names | `IDI_LOGO_{WHITE,RED,GRAY,BLUE}` | `com.fxsound.FxSound-{on,processing,off}` (+ `-symbolic`) in `hicolor/**/status/` |
| D-Bus well-known name | n/a | `com.fxsound.FxSound` (single-instance + control) |
| SNI object | Shell_NotifyIcon GUID | `/StatusNotifierItem` on that bus name |
| Autostart | Startup-folder `.lnk` + `HKCU\…\Run` value `FxSound` | `~/.config/autostart/com.fxsound.FxSound.desktop` |
| Log | `%APPDATA%\FxSound\fxsound.log` | `$XDG_STATE_HOME/fxsound/fxsound.log` (default `~/.local/state/fxsound/`) |
| Crash dump | `%APPDATA%\FxSound\fxsound.dmp` | `$XDG_STATE_HOME/fxsound/crash/<ts>.dmp` |
| Settings | `%APPDATA%\FxSound\FxSound.settings` | `$XDG_CONFIG_HOME/fxsound/settings.toml` |
| Machine defaults | `%ProgramData%\FxSound\FxSound.settings` | `/etc/fxsound/defaults.toml`, then `/usr/share/fxsound/defaults.toml` |
| Status file | `%APPDATA%\FxSound\status.json` | `$XDG_RUNTIME_DIR/fxsound/status.json` (plus stdout, §4.6) |
| Presets | `%APPDATA%\FxSound\Presets\*.fac` | `$XDG_DATA_HOME/fxsound/presets/*.fac` |
| Auto-saved presets | `%APPDATA%\FxSound\AutoSave\*.fac` | `$XDG_DATA_HOME/fxsound/autosave/*.fac` |
| Factory presets | `<cwd>\Factsoft\*.fac` | `/usr/share/fxsound/factsoft/*.fac` (search `$XDG_DATA_DIRS`) |

Minimum `.desktop` file (must match the Wayland `app_id` exactly so Hyprland rules bind):

```ini
[Desktop Entry]
Type=Application
Name=FxSound
GenericName=Audio Enhancer
Comment=System-wide audio enhancement and equalizer
Exec=fxsound
Icon=com.fxsound.FxSound
Terminal=false
Categories=AudioVideo;Audio;Mixer;
Keywords=equalizer;eq;audio;dsp;bass;
StartupNotify=true
StartupWMClass=com.fxsound.FxSound
X-GNOME-UsesNotifications=true
SingleMainWindow=true
```

Hyprland rules then key off `class:` (Hyprland matches the Wayland `app_id` against
`class`):

```
windowrulev2 = float,       class:^(com\.fxsound\.FxSound)$
windowrulev2 = size 800 600,class:^(com\.fxsound\.FxSound)$
windowrulev2 = center,      class:^(com\.fxsound\.FxSound)$
```

---

## 2. Process lifecycle

### 2.1 Cold-start sequence — exact order

`START_JUCE_APPLICATION(FxSoundApplication)` (`Main.cpp:331`) generates `WinMain`;
JUCE then calls `initialise(commandline)` (`Main.cpp:49`). The whole body is wrapped in
`try { … } catch (const std::exception&) { … } catch (...) { … }` (`Main.cpp:52,75,84`).

```
 1. SetUnhandledExceptionFilter(unhandledExceptionFilter)            Main.cpp:54
 2. CoInitializeEx(0, COINIT_MULTITHREADED)                          Main.cpp:56
 3. if SUCCEEDED: CoInitializeSecurity(NULL, -1, NULL, NULL,
       RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IMPERSONATE,
       NULL, EOAC_NONE, NULL)                                        Main.cpp:59-60
 4. LookAndFeel::setDefaultLookAndFeel(&theme_)                      Main.cpp:63
 5. setWorkingDirectory()   // chdir to the exe's directory          Main.cpp:65,308-321
 6. FxController::getInstance()  <-- singleton ctor runs here        Main.cpp:67
      ├ MessageWindow(L"FxSoundHotkeys", eventCallback)              FxController.cpp:126
      ├ FileLogger::createDefaultAppLogger("FxSound","fxsound.log")  FxController.cpp:154
      ├ log "v<version>" and OS name and arch (x86/x64/ARM64)        FxController.cpp:155-171
      ├ view_ = settings "view", clamp to {1,2} else Pro             FxController.cpp:173-181
      ├ hotkeys = settings "hotkeys" && SysInfo::canSupportHotkeys() FxController.cpp:182
      │   └ if true -> registerHotkeys()  (5 × RegisterHotKey)       FxController.cpp:186, 2820-2866
      ├ menu_clicked, always_on_top, hide_help_tooltips,
      │   hide_notifications, automatic_updates(default true)        FxController.cpp:188-193
      ├ max_user_presets, clamped to [10,120] else forced 120        FxController.cpp:194-199
      └ ProcessIdToSessionId + WTSRegisterSessionNotification
            (NOTIFY_FOR_THIS_SESSION)                                FxController.cpp:203-205
 7. FxController::initConfig(commandline)      // see §4.3           Main.cpp:67
 8. audio_passthru_ = make_unique<AudioPassthru>()                   Main.cpp:69
 9. main_window_   = make_unique<FxMainWindow>()                     Main.cpp:70
10. system_tray_view_.reset(new FxSystemTrayView())                  Main.cpp:71
      ├ FxModel::addListener(this)                                   FxSystemTrayView.cpp:30
      ├ custom_notification_ = true                                  FxSystemTrayView.cpp:32
      ├ addToDesktop(0)  // hidden 0-size message window             FxSystemTrayView.cpp:34
      ├ subclass wndProc, stash `this` in GWLP_USERDATA              FxSystemTrayView.cpp:38-40
      ├ RegisterWindowMessage("TaskbarCreated")                      FxSystemTrayView.cpp:42
      └ addIcon()  -> Shell_NotifyIcon(NIM_ADD) + NIM_SETVERSION 4   FxSystemTrayView.cpp:44,172-214
11. FxController::init(main_window, tray, audio_passthru)            Main.cpp:73
      ├ guard: only runs when !isTimerRunning()                      FxController.cpp:698
      ├ audio_passthru_->init() != 0 -> AlertWindow
      │   "Error in system audio configuration. Unable to run FxSound"
      │   then systemRequestedQuit()                                 FxController.cpp:704-711
      ├ version-changed branch (prev_version != app_version):        FxController.cpp:713-723
      │    RegDeleteTree(HKCU, "Software\DFX")                       FxController.cpp:717
      │    pushMessage(" ", {"Click here to see what's new on this
      │        version!", "https://www.fxsound.com/changelog"})      FxController.cpp:719
      │    settings "version" = app_version; "run_minimized" = false FxController.cpp:721-722
      ├ setDspProcessingModule, getSoundDevices(false), initOutputs  FxController.cpp:725-727
      ├ !dfx_enabled_ && !isRemoteSession -> modal FxDeviceErrorMessage
      │    then systemRequestedQuit()                                FxController.cpp:729-736
      ├ registerCallback(this)                                       FxController.cpp:738
      ├ create %APPDATA%\FxSound\Presets                             FxController.cpp:740-744
      ├ setPowerState(settings "power")                              FxController.cpp:746
      ├ initPresets()  (Factsoft/*.fac + user Presets/*.fac)         FxController.cpp:748
      ├ setPreset(settings "preset")                                 FxController.cpp:750-751
      ├ showView()  (Pro or Lite)                                    FxController.cpp:753
      ├ theme_mode = settings "theme_mode" clamped to [0,NumModes)   FxController.cpp:755-770
      ├ survey_tip_ = !settings "survey_displayed"                   FxController.cpp:772
      ├ **if !settings "run_minimized" -> showMainWindow()
      │   else -> hideMainWindow()**                                 FxController.cpp:774-781
      ├ main_window_->setIcon(power,false); tray->setStatus(power,false) FxController.cpp:784-785
      └ RegisterSuspendResumeNotification(msgwnd, DEVICE_NOTIFY_WINDOW_HANDLE)
                                                                     FxController.cpp:787-798
```

On any exception in steps 1–11 the handler logs `std::exception: <what>` or
`Unknown exception`, calls `CaptureAndLogCallStack()` and `quit()`
(`Main.cpp:75-92`).

Note the 100 ms master timer is **not** started here: it is started by `powerOn(true)`
(`FxController.cpp:1727-1735`, `startTimer(100)`) and stopped by `powerOn(false)`
(`:1739-1744`). `FxController::init` is a no-op if the timer is already running
(`:698`) — i.e. `init` is idempotent-by-accident.

### 2.2 Linux translation of the startup steps

| Windows step | Linux |
|---|---|
| `SetUnhandledExceptionFilter` | `crash-handler` + `minidumper` crates, or a `SIGSEGV/SIGBUS/SIGILL/SIGABRT/SIGFPE` handler writing a backtrace. See §2.5. |
| `CoInitializeEx(COINIT_MULTITHREADED)` + `CoInitializeSecurity` | Nothing. COM does not exist. The analogous "connect to the session bus" step is `zbus::Connection::session().await` — do it once, share the connection between the SNI, notifications and logind listeners. |
| `LookAndFeel::setDefaultLookAndFeel` | Install the egui `Style`/`Visuals` from spec 0x (theme). Must happen before the first frame. |
| `setWorkingDirectory()` (chdir to exe dir) | **Do not port.** See §2.3. |
| `FxController` singleton | A `AppState` owned by the event loop, not a global. Avoid `DeletedAtShutdown`-style leak-on-exit (`FxController.h:42` derives from `DeletedAtShutdown`, and `getInstance()` leaks a `new FxController` deliberately at `:60-64`). |
| `MessageWindow(L"FxSoundHotkeys")` | An invisible Win32 message-only window exists **only** to receive `WM_HOTKEY`, `WM_POWERBROADCAST`, `WM_WTSSESSION_CHANGE`. On Linux these are three separate D-Bus subscriptions; no window is needed. |
| `WTSRegisterSessionNotification` | `org.freedesktop.login1` session `Lock`/`Unlock` signals + `Active` property. §2.6 |
| `RegisterSuspendResumeNotification` | `org.freedesktop.login1.Manager` `PrepareForSleep(bool)` signal. §2.6 |
| Tray icon `Shell_NotifyIcon(NIM_ADD)` | `ksni` StatusNotifierItem registration. §5.8 |

### 2.3 The working-directory hack — do not port it

`setWorkingDirectory()` (`Main.cpp:308-321`) does
`GetModuleFileName(NULL,…)` → strip after the last `\` or `/` → `SetCurrentDirectory`.
Its sole purpose is that `initPresets()` resolves factory presets relative to the CWD:

```cpp
auto working_dir = File::getCurrentWorkingDirectory();
FileSearchPath preset_search_path(File::addTrailingSeparator(working_dir.getFullPathName()) + L"Factsoft");
```
`FxController.cpp:836-838`

**Linux:** never `chdir()` in a GUI app — it breaks relative paths the user passed on
the command line and confuses file dialogs. Resolve factory presets from a compiled-in
prefix (`option_env!("FXSOUND_DATADIR")`, default `/usr/share/fxsound`) and then walk
`$XDG_DATA_DIRS` for `fxsound/factsoft`. Keep the process CWD untouched.

### 2.4 Shutdown sequence

`shutdown()` (`Main.cpp:95-125`) runs only the guarded block when
`main_window_ != nullptr` (`Main.cpp:97`):

```
1. FxController::autoSaveModifiedPreset()      Main.cpp:99   (-> FxController.cpp:989-994)
2. FxController::stopTimer()                   Main.cpp:100
3. audio_passthru_.reset()                     Main.cpp:102
4. UiaDisconnectAllProviders() if UIAutomationCore.dll is loaded
                                               Main.cpp:109-114
5. system_tray_view_.reset()                   Main.cpp:116
     -> removeListener, restore original WNDPROC, clear GWLP_USERDATA,
        Shell_NotifyIcon(NIM_DELETE, NIF_GUID), removeFromDesktop()
                                               FxSystemTrayView.cpp:47-62
6. main_window_.reset()                        Main.cpp:118
then unconditionally:
7. LocalisedStrings::setCurrentMappings(nullptr)   Main.cpp:121
8. LookAndFeel::setDefaultLookAndFeel(nullptr)     Main.cpp:122
9. CoUninitialize()                                Main.cpp:124
```

`FxController::~FxController` additionally unregisters the suspend/resume notification,
stops the timer, `WTSUnRegisterSessionNotification`, `unregisterHotkeys()`
(`FxController.cpp:208-220`).

Quit entry points:

| Trigger | Path |
|---|---|
| Tray **Exit** | `exitClicked` → `FxController::exit()` (`FxSystemTrayView.cpp:285-287`) → `autoSaveModifiedPreset(); systemRequestedQuit(); return true` (`FxController.cpp:996-1005`) |
| `systemRequestedQuit()` | unconditional `quit()` (`Main.cpp:128-134`) — the app **never** refuses a quit request |
| Audio init failure | `FxController.cpp:709` |
| Driver/device error dialog dismissed | `FxController.cpp:734` |
| Exception during `initialise` | `Main.cpp:82,91` |

**Window close ≠ quit.** `userTriedToCloseWindow()` and `closeButtonPressed()` both call
`hideMainWindow()` (`FxMainWindow.cpp:608-616`). The only way to quit from the UI is the
tray **Exit** item; the hamburger menu has *no* Exit item (`FxMainWindow.cpp` menu, §5.5).

**Linux:**
- Map "tray Exit" → graceful shutdown: stop the PipeWire stream/filter node, autosave the
  modified preset, unregister the SNI, drop the D-Bus connection, remove the
  single-instance socket, then exit(0).
- Also handle `SIGINT`/`SIGTERM` (the Windows build has no equivalent since it's a GUI
  subsystem binary) → same graceful path. `SIGHUP` from logind on session end likewise.
- There is no `CoUninitialize`/UIA analogue; the `UiaDisconnectAllProviders` block
  (`Main.cpp:104-114`) exists purely to tame Windows accessibility COM refcounts.
  On Linux, AT-SPI is out of scope for the first port (flag as a gap: egui has no
  AT-SPI bridge; `accesskit` + `accesskit_unix` is the path if accessibility is required).

### 2.5 Crash path

`unhandledExceptionFilter` (`Main.cpp:145-188`):

1. `logPath = <userApplicationDataDirectory>/FxSound`, created if missing (`:149-152`).
2. Dump file `fxsound.dmp` in that directory, `CreateFileW(GENERIC_WRITE, 0, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL)` (`:154-155`).
3. `MiniDumpWriteDump(…, MiniDumpNormal, &dumpInfo, nullptr, nullptr)` with `ThreadId = GetCurrentThreadId()`, `ClientPointers = FALSE` (`:159-170`).
4. Logs `"Unhandled exception\nException code: 0x%X\nException flags: 0x%X\nException address: 0x%p\n"` (`:175-180`).
5. `CaptureAndLogCallStack(exception_info->ContextRecord)` (`:185`).
6. Returns `EXCEPTION_EXECUTE_HANDLER` → the process dies (`:187`).

`CaptureAndLogCallStack` (`Main.cpp:190-306`): `MAX_FRAMES = 64` (`Main.cpp:143`),
`SymSetOptions(SYMOPT_LOAD_LINES | SYMOPT_UNDNAME)`, `SymInitialize(process,NULL,TRUE)`
(`:198-199`). With no context it uses `CaptureStackBackTrace`; with a crash context it
uses `StackWalk64` with per-architecture seeding:

| Arch | machineType | AddrPC | AddrFrame | AddrStack | Lines |
|---|---|---|---|---|---|
| `_M_IX86` | `IMAGE_FILE_MACHINE_I386` | `Eip` | `Ebp` | `Esp` | `Main.cpp:238-244` |
| `_M_X64` | `IMAGE_FILE_MACHINE_AMD64` | `Rip` | **`Rsp`** | `Rsp` | `Main.cpp:246-252` |
| `_M_ARM64` | `IMAGE_FILE_MACHINE_ARM64` | `Pc` | `Fp` | `Sp` | `Main.cpp:254-260` |

(The x64 `AddrFrame = Rsp` at `Main.cpp:250` is unusual — `Rbp` is the conventional frame
pointer — but it is what the code does; it is cosmetic since x64 `StackWalk64` is
unwind-table driven.)

Symbol lines are formatted `"%S at 0x%llX\n"` and, when line info resolves,
`"    File: %S, Line: %d\n"` with the filename basename taken after the last `\`
(`Main.cpp:216-224, 285-293`).

**Linux equivalent (be specific):**

- **No `MiniDumpWriteDump`.** Use the `crash-handler` crate to install handlers for
  `SIGSEGV, SIGABRT, SIGBUS, SIGILL, SIGFPE, SIGTRAP`, and `minidumper` to write a real
  Breakpad-format minidump **from a separate monitor process** (writing a dump from
  inside a crashed process is not async-signal-safe; this is the whole reason
  `minidumper` has a client/server split). Dump path:
  `$XDG_STATE_HOME/fxsound/crash/<unix_ts>-<pid>.dmp`.
- A cheap fallback for a first cut: a `signal-hook` handler that writes
  `backtrace::Backtrace::new_unresolved()` frames to `$XDG_STATE_HOME/fxsound/fxsound.log`
  using only `write(2)` on a pre-opened fd.
- Symbolisation is offline (`addr2line`/`minidump-stackwalk`), unlike DbgHelp's in-process
  `SymFromAddr`. Ship a `.dwp`/debug-link or push to a debuginfod server.
- Also install `std::panic::set_hook` to route Rust panics to the same log before abort.
- Register the app with `systemd-coredump`-friendly limits; do **not** try to replicate
  Windows' "write the dump then die" ordering beyond what `crash-handler` gives you.

### 2.6 Session and power events

`FxController::eventCallback` (`FxController.cpp:1916-2050`) is the WNDPROC of the hidden
`FxSoundHotkeys` window.

`WM_POWERBROADCAST` (`FxController.cpp:2017-2028`):

| wParam | Action |
|---|---|
| `PBT_APMSUSPEND` | `onSystemSuspend()` → if power on, `audio_passthru_->mute(true)` (`:2145-2151`) |
| `PBT_APMRESUMESUSPEND` or `PBT_APMRESUMEAUTOMATIC` | `onSystemResume()` → if power on, `audio_passthru_->mute(false)` (`:2153-2159`) |

`WM_WTSSESSION_CHANGE` (`FxController.cpp:2030-2047`):

| wParam | Action |
|---|---|
| login event (`WTS_SESSION_DESKTOP_READY`, or `WTS_CONSOLE_CONNECT` on Windows 7) | `setPowerState(settings "power")` (`:2033-2037`) |
| `WTS_SESSION_UNLOCK` | same as above (`:2035`) |
| `WTS_CONSOLE_DISCONNECT` | `powerOn(false)` — note: DSP off **without** touching the persisted power setting (`:2039-2041`) |
| `WTS_REMOTE_CONNECT` | `setPowerState(settings "power")` (`:2043-2045`) |

Everything else falls through to `DefWindowProc` (`:2049`).

Device-change callbacks are ignored if they arrive for a different session:
`if (session_id_ != WTSGetActiveConsoleSessionId()) return;` (`FxController.cpp:2121-2122`).

**Linux equivalents (opinionated):**

| Windows | Linux, via `zbus` on the **system** bus (`org.freedesktop.login1`) |
|---|---|
| `PBT_APMSUSPEND` / resume | `Manager.PrepareForSleep(b start)` signal: `true` → about to sleep, `false` → resumed. Take a **delay inhibitor** (`Manager.Inhibit("sleep","FxSound","Pausing audio processing","delay")`) so you get a window to deactivate the PipeWire stream before suspend; release the fd to let the suspend proceed. |
| `WTS_SESSION_LOCK/UNLOCK` | `Session.Lock` / `Session.Unlock` signals on your own session object, or the `LockedHint` property. |
| `WTS_CONSOLE_DISCONNECT` (VT switch away / fast user switch) | `Session.Active` property change → `false`. Mirror the Windows behaviour: stop DSP (`powerOn(false)`) **without** rewriting the stored `power` setting. |
| `WTSGetActiveConsoleSessionId` guard | Compare `Manager.GetSessionByPID(getpid())` against `Seat.ActiveSession`. |
| `SM_REMOTESESSION` (`SysInfo.cpp:138-141`, gates the power toggle — `FxSystemTrayView.cpp:303`, `FxController.cpp:367`, `FxController.cpp:1008`) | `Session.Remote` boolean property on logind. Keep the same gate: when remote, force power off and disable the tray **Turn On/Off** item. |

The Windows "mute on suspend" maps to `pw_stream_set_active(stream, false)` (or removing
the filter-chain node's link) rather than a gain mute — it avoids a wake-up glitch and
lets the graph re-negotiate the format after resume.

### 2.7 Global hotkeys registered at startup

Registered in the `FxController` ctor when `settings "hotkeys" && SysInfo::canSupportHotkeys()`
(`FxController.cpp:182-187`); `canSupportHotkeys()` unconditionally returns `true`
(`SysInfo.cpp:133-136`).

`registerHotkeys()` (`FxController.cpp:2820-2866`) calls `::RegisterHotKey(msgwnd, id, mod, vk)`
for five commands; `unregisterHotkeys()` mirrors it (`:2868-2880`).

| Command id | Const | Settings key | Default value | Decoded |
|---|---|---|---|---|
| `1001` | `CMD_ON_OFF` | `cmd_on_off` | `393297` (`Settings.cpp:34`) | mod `6` = CONTROL\|SHIFT, vk `0x51` = **Ctrl+Shift+Q** |
| `1002` | `CMD_OPEN_CLOSE` | `cmd_open_close` | `393285` (`Settings.cpp:35`) | vk `0x45` = **Ctrl+Shift+E** |
| `1003` | `CMD_NEXT_PRESET` | `cmd_next_preset` | `393281` (`Settings.cpp:36`) | vk `0x41` = **Ctrl+Shift+A** |
| `1004` | `CMD_PREVIOUS_PRESET` | `cmd_previous_preset` | `393306` (`Settings.cpp:37`) | vk `0x5A` = **Ctrl+Shift+Z** |
| `1005` | `CMD_NEXT_OUTPUT` | `cmd_change_output` | `393303` (`Settings.cpp:38`) | vk `0x57` = **Ctrl+Shift+W** |

Ids are `FxController.h:216-220`. Encoding: `mod = (value >> 16) & 0x7`, `vk = value & 0xff`
(`FxController.cpp:2177-2179`). A hotkey is accepted only if
`mod == (MOD_CONTROL|MOD_ALT)` (3) or `mod == (MOD_CONTROL|MOD_SHIFT)` (6) **and**
`vk` is `'0'..'9'` or `'A'..'Z'` (`FxController.cpp:2180-2183`). All five defaults
use mod 6 (Ctrl+Shift).

Behaviours (`FxController.cpp:1923-2014`): on/off toggles power and pushes
`"FxSound is %s."` with `on`/`off`; open/close toggles the main window; next/previous
preset wrap around the preset list; next output cycles `active_output_devices_` skipping
any device with `deviceNumChannel < 2`.

**Linux: global hotkeys are not available to a Wayland client. Full stop.**
There is no Wayland protocol a normal client can use to grab a system-wide key
combination (that is deliberate — it would be a keylogging primitive). Three
substitutes, in order of preference:

1. **Compositor keybinding → CLI.** Ship documented snippets; the CLI already *is* a
   remote-control protocol (§4). Hyprland:
   ```
   bind = CTRL SHIFT, Q, exec, fxsound --power=toggle   # see §4.7: add a toggle value
   bind = CTRL SHIFT, E, exec, fxsound --toggle-window
   bind = CTRL SHIFT, A, exec, fxsound --next-preset
   bind = CTRL SHIFT, Z, exec, fxsound --prev-preset
   bind = CTRL SHIFT, W, exec, fxsound --next-output
   ```
   (Sway `bindsym`, KDE custom shortcuts, GNOME `custom-keybindings` are equivalent.)
2. **XDG Global Shortcuts portal** — `org.freedesktop.portal.GlobalShortcuts` (available
   under KDE Plasma ≥ 5.27 and in newer xdg-desktop-portal-wlr/hyprland). It lets the app
   *request* shortcuts which the **compositor** binds and which the user can re-map in
   system settings. Use this when present; it is the only in-app path that works on Wayland.
3. Under X11/XWayland only, `XGrabKey` still works — do not build the feature around it.

The Settings dialog's hotkey editor (`FxHotkeyLabel`, five rows named
`"Turn FxSound On/Off"`, `"Open/Close FxSound"`, `"Use Next Preset"`,
`"Use Previous Preset"`, `"Change Playback Device"` — `FxSettingsDialog.cpp:344-345`)
must therefore become either a *portal request* UI or a read-only page that shows the
recommended compositor snippets with a copy button.

### 2.8 The 100 ms master timer

Started by `powerOn(true)` → `startTimer(100)` (`FxController.cpp:1732-1735`),
stopped by `powerOn(false)` (`:1741-1743`). `timerCallback()` (`FxController.cpp:2052-2116`):

- Skips one tick after an output change (`output_changed_` → `Thread::sleep(200)`, `:2054-2059`).
- Polls `dfx_dsp_.getTotalAudioProcessedTime()`; **5 consecutive ticks (≈500 ms)** of change
  flips `audio_process_on_ = true`; 5 consecutive ticks of no change flips it back
  (`:2061-2088`). This is what drives the tray icon's "processing" state (§5.2).
- Auto-saves a modified preset every `AUTO_SAVE_INTERVAL = 600` ticks = **60 s** (`:2113-2122`… actually `:2114-2121`).
- Fires `checkUpdates()` at exactly **10:00:00 local** if `automatic_updates`
  (`:2111-2115`), which spawns `updater.exe /silent` at most once per 24 h
  (`FxController.cpp:2611-2626`).

**Linux:** do not poll at 100 Hz from the UI thread. Run the "is audio flowing" detector on
the PipeWire thread (count processed frames in the filter callback, publish an
`AtomicU64`), and let the egui side sample it once per repaint. Keep the same
**5-sample / 500 ms** hysteresis so the tray icon does not flicker. The update check
(`updater.exe`) has no Linux analogue — packages are updated by the distro; drop it, or
replace with a "check for a newer release" HTTP call guarded by a config key
(`automatic_updates`, default `true` — `FxController.cpp:193`).

---

## 3. Single-instance enforcement and command-line forwarding

### 3.1 Windows mechanism

```cpp
bool moreThanOneInstanceAllowed() override { return false; }   // Main.cpp:46
void anotherInstanceStarted (const String& commandline) override
{
    FxController::getInstance().applyConfig(commandline);      // Main.cpp:136-139
}
```

The transport is entirely inside JUCE (`JUCEApplicationBase` / `MultipleInstanceHandler`),
which is **not in this tree**. The observable contract, corroborated by the in-tree Go
client, is:

- The second process starts, discovers a live first instance, hands it the **raw**
  command-line string, and **exits almost immediately** —
  *"forward their command line to an already-running instance over a Windows message
  broadcast and exit almost immediately"* (`fxmcp/internal/fxsound/process.go:84-87`).
- The first instance receives the string on its message thread and calls
  `anotherInstanceStarted` → `applyConfig`.
- The command line is **not** parsed argv. JUCE re-derives it from the raw tail of
  `GetCommandLineW()` via `CharacterFunctions::findEndOfToken`, and `FxController`
  re-tokenises it with `StringArray::fromTokens` — a quote-toggling state machine with
  **no backslash escapes**: `"` always flips in/out of a quoted region
  (`fxmcp/internal/fxsound/process.go:39-62`). This is why the Go client builds the raw
  command line itself and **rejects** any value containing a literal `"`, calling it an
  argument-injection path (`fxmcp/internal/fxsound/config.go:31-45, 58-60`).
- There is no reply channel. `--status` answers out-of-band by writing a file
  (§4.6), and the running instance additionally does `AttachConsole(ATTACH_PARENT_PROCESS)`
  to print to *its own* original console — explicitly **not** the forwarding process's
  console (`FxController.cpp:686-699`).

### 3.2 Observable contract the port must preserve

| Property | Value |
|---|---|
| Second instance exits | yes, immediately, exit code 0 |
| Forwarded payload | the full command line, verbatim |
| Handler on the live instance | `applyConfig` (a *different, larger* parser than cold start's `initConfig`) |
| Side effect of any forwarded command line without `--run_minimized` | the main window is shown and raised (`FxController.cpp:523-531`) |
| `--status` | returns early before that, so it does **not** raise the window (`FxController.cpp:348-352`) |
| Client-side timeout expectation | 5 s for an apply, 3 s for the process to appear, 8 s for it to become ready (`fxmcp/internal/fxsound/config.go:12-18`) |

### 3.3 Linux design — single instance

**Recommendation: an abstract `AF_UNIX` `SOCK_SEQPACKET` socket, with a
`$XDG_RUNTIME_DIR` lock file as the fallback and as the PID record.**

Why abstract first:
- an abstract socket has no filesystem inode, so it **disappears when the owning process
  dies** — no stale-socket cleanup, no "is this socket alive?" dance, which is the classic
  failure mode of `$XDG_RUNTIME_DIR/app.sock`;
- `SOCK_SEQPACKET` gives message framing for free (each `send` is one command line).

Why the fallback is still required:
- abstract sockets live in the **network namespace**, not the user session, so two users
  on one machine share the namespace — the name must contain the uid, and you must verify
  the peer with `SO_PEERCRED`;
- Flatpak/containers with a private netns cannot reach the host's abstract socket. In a
  sandbox, `$XDG_RUNTIME_DIR` **is** shared, so fall back to a filesystem socket there.

```
primary  : abstract  "\0fxsound/<uid>/<XDG_SESSION_ID or WAYLAND_DISPLAY>"
fallback : $XDG_RUNTIME_DIR/fxsound/instance.sock  (dir 0700)
lock     : $XDG_RUNTIME_DIR/fxsound/instance.lock  (flock LOCK_EX|LOCK_NB, holds "<pid>\n")
```

Startup algorithm:

```
1. mkdir -p $XDG_RUNTIME_DIR/fxsound (0700)
2. open instance.lock, flock(LOCK_EX|LOCK_NB)
     ok    -> we are the primary. write our pid. bind the socket. listen. continue booting.
     EWOULDBLOCK -> a primary exists. goto 3.
3. connect to the abstract socket (fallback: the filesystem socket), 200 ms timeout
     fail  -> primary is wedged or dying: retry the flock 5× over 1 s, then
              print "FxSound is already running but not responding" to stderr, exit 1
     ok    -> send the request frame, read the reply frame (5 s timeout), print it, exit 0
```

Wire protocol — one JSON object per SEQPACKET datagram, so the ambiguity that forced
fxmcp to hand-build a raw Windows command line simply cannot arise:

```jsonc
// request (client -> primary)
{ "v": 1, "argv": ["--preset=Bass Booster", "--power=1"], "cwd": "/home/u", "env_lang": "fr" }
// reply (primary -> client)
{ "v": 1, "ok": true, "stdout": "", "stderr": "" }
// for --status, "stdout" carries the whole status JSON document
```

Rules for the port:

- **Pass real `argv`**, never a re-joined string. That deletes the entire class of quoting
  bugs documented at `fxmcp/internal/fxsound/process.go:39-62` and lets a preset name
  contain `"`.
- Verify `SO_PEERCRED.uid == geteuid()` on accept; reject otherwise.
- Answer synchronously. `--status` should print to **the client's stdout** (the Windows
  `AttachConsole` hack at `FxController.cpp:686-699` exists only because Windows had no
  reply channel) while still writing `$XDG_RUNTIME_DIR/fxsound/status.json` for
  compatibility with a poll-based client.
- Handle the request on the main thread (queue it into the winit event loop with an
  `EventLoopProxy` user event) so it can touch the UI safely.
- If a D-Bus session bus is present, **also** request the well-known name
  `com.fxsound.FxSound` and expose the same commands as methods — it makes the app
  scriptable with `busctl`/`gdbus` and makes `SingleMainWindow=true` / activation work
  with desktop shells. Name acquisition failure (`NameExists`) is a second, independent
  single-instance signal. Do not rely on D-Bus **alone**: the app must work in a bare
  Hyprland session with no session bus.
- Do **not** use the X11 `_NET_WM_PID`/window-search trick, and do not use
  `SingleInstance` helpers that depend on X11.

Recommended crates: `rustix` or `nix` for the socket + `flock` + `SO_PEERCRED`,
`serde_json` for the frame, `winit::event_loop::EventLoopProxy` for handoff.

---

## 4. Command line

### 4.1 Parsing rules (normative)

From `docs/COMMAND_LINE_OPTIONS.md`:

| Rule | Source |
|---|---|
| Value must be attached with `=`. `--power 1` parses as two unrelated arguments and the value is **silently ignored**. | `docs/COMMAND_LINE_OPTIONS.md:7` |
| Numeric values outside range are **silently reset to the default**, no error. | `:9` |
| `--balance`, `--master_gain` are rounded to the nearest whole number. | `:11`, and `FxController.cpp:1803` (`std::round`), `:1815` |
| `--filter_q`, `--volume_leveling` are rounded to the nearest `0.5`. | `:11`, and `FxController.cpp:1827` (`std::round(x*2)/2`), `:1791` |
| Values with spaces must be double-quoted right after `=`. | `:13` |
| A literal `"` inside a value corrupts the parse (client-side rejection required). | `fxmcp/internal/fxsound/config.go:31-45` |

`ArgumentList` is constructed from the executable's file name plus the raw command line:
`ArgumentList(File::getSpecialLocation(invokedExecutableFile).getFileName(), commandline)`
(`FxController.cpp:223` and `:346`).

### 4.2 Full option table

`C` = honoured on cold start (`initConfig`), `R` = honoured on a running instance
(`applyConfig`).

| Option | Value | C | R | Effect / validation | Source |
|---|---|---|---|---|---|
| `--power=<0\|1>` | `0`=off `1`=on | ✔ | ✔ | Cold: writes setting `power`. Running: `setPowerState()`, **skipped entirely if `SysInfo::isRemoteSession()`**. Any non-zero int ⇒ on. | `FxController.cpp:241-247`, `:367-373`; doc `:17` |
| `--preset=<name>` | exact, case-sensitive | ✔ | ✔ | Cold: writes setting `preset` (applied later in `init`). Running: only if power on; `setPreset(name)`. | `:249-252`, `:395-401`; doc `:18` |
| `--save_preset=<name>` | new name | ✘ | ✔ | Needs power on, preset modified, `userPresetCount < max_user_presets`, name non-empty after sanitising. | `:402-412`; doc `:19,42` |
| `--overwrite_preset` | flag | ✘ | ✔ | Needs power on, modified, selected preset is a `UserPreset`. | `:413-420`; doc `:20` |
| `--undo_preset` | flag | ✘ | ✔ | Needs power on and modified. | `:421-427`; doc `:21` |
| `--rename_preset=<name>` | new name | ✘ | ✔ | Needs power on, **not** modified, selected is `UserPreset`, sanitised name non-empty. | `:428-440`; doc `:22` |
| `--delete_preset` | flag | ✘ | ✔ | Needs power on and selected is `UserPreset`. | `:441-448`; doc `:23` |
| `--output=<device>` | exact friendly name | ✔ | ✔ | Cold: `setOutputName()` (setting `output_device_name`). Running: also scans `getSoundDevices()` for an exact name match and `setOutput(id)`. | `:254-257`, `:451-462`; doc `:24` |
| `--view=<1\|2>` | `1`=Lite `2`=Pro/Full | ✔ | ✔ | Accepted only if exactly 1 or 2; writes setting `view`; running instance also `showView()`. | `:259-267`, `:507-516`; doc `:25` |
| `--language=<code>` | e.g. `en`, `fr`, `fi` | ✔ | ✔ | Cold: if absent falls back to setting `language`, then `SystemStats::getDisplayLanguage()`. | `:269-278`, `:518-521`; doc `:26` |
| `--num_bands=<n>` | one of `5,10,15,20,31` | ✔ | ✔ | Anything else ⇒ `DEFAULT_NUM_EQ_BANDS = 10`. Cold falls back to setting `num_bands`. | `:283-293`, `:468-473`; `FxController.h:45`; doc `:27` |
| `--balance=<n>` | `-20.0…+20.0` dB | ✔ | ✔ | Out of range ⇒ `DEFAULT_BALANCE = 0.0f`; rounded to integer. | `:307-317`, `:484-489`; `FxController.h:48`; doc `:28` |
| `--filter_q=<n>` | `1.0…3.0` | ✔ | ✔ | Out of range ⇒ `DEFAULT_FILTER_Q = 1.0f`; rounded to 0.5. | `:319-329`, `:492-497`; `FxController.h:49`; doc `:29` |
| `--master_gain=<n>` | `-20.0…+20.0` dB | ✔ | ✔ | Out of range ⇒ `DEFAULT_MASTER_GAIN = 0.0f`; rounded to integer. | `:331-341`, `:500-505`; `FxController.h:50`; doc `:30` |
| `--volume_leveling=<n>` | `0.0…4.0` dB | ✔ | ✔ | Out of range ⇒ `DEFAULT_VOLUME_LEVELING = 0.0f`; rounded to 0.5. | `:295-305`, `:476-481`; `FxController.h:47`; doc `:31` |
| `--set_band_freq=<b:f[,b:f…]>` | 0-based band : Hz | ✘ | ✔ | Whole list ignored unless `pairs.size() <= getNumEqBands()`. Per-pair band/freq range checks live in `setEqBandFrequency`. | `:536-553`; doc `:32` |
| `--set_band_gain=<b:g[,b:g…]>` | 0-based band : dB `-12.0…+12.0` | ✘ | ✔ | Same size guard. `MIN_GAIN=-12.0f`, `MAX_GAIN=12.0f`. | `:555-572`; `FxController.h:51-52`; doc `:33` |
| `--set_effect=<name:v[,name:v…]>` | `v` = `0.0…10.0` | ✘ | ✔ | Whole list ignored unless `pairs.size() <= 5`. Names (lower-cased): `fidelity`\|`clarity`, `ambience`, `surround`, `dynamicboost`\|`dynamic_boost`, `bass`\|`bassboost`\|`bass_boost`. Out-of-range value is **left unchanged** (early `return` in `setEffectValue`), not defaulted. | `:574-600`, `:1756-1760`; doc `:34` |
| `--status` | flag | ✘ | ✔ | Writes `status.json`, prints to the *running* instance's own parent console, then **returns immediately — every other option on the same line is ignored and the window is not raised**. | `:348-352`, `:635-700`; doc `:35` |
| `--run_minimized` | flag | ✔ | ✔ | Cold: sets `run_minimized=true` before the window exists. Running: sets it **and** hides the window. | `:236-239`, `:523-531`; doc `:36` |

### 4.3 `initConfig` — cold start, exact order

`FxController::initConfig` (`FxController.cpp:221-342`). It reads ten options
(`:225-234`) and *never* sees `--status`, `--set_band_*`, `--set_effect` or any preset
management command.

```
1.  --run_minimized     -> settings.run_minimized = true                :236-239
2.  --power             -> settings.power = (int != 0)                  :241-247
3.  --preset            -> settings.preset = <name>                     :249-252
4.  --output            -> setOutputName(<name>)                        :254-257
5.  --view              -> settings.view + view_ (only if 1 or 2)       :259-267
6.  --language, else settings.language, else system display language;
    then setLanguage(language)                                          :269-278
7.  --num_bands        else settings.num_bands;        validate; set    :283-293
8.  --volume_leveling  else settings.volume_leveling;  validate; set    :295-305
9.  --balance          else settings.balance;          validate; set    :307-317
10. --filter_q         else settings.filter_q;         validate; set    :319-329
11. --master_gain      else settings.master_gain;      validate; set    :331-341
```

Steps 7–11 are the *state restore* path: with no CLI at all they replay the last saved
EQ globals into the DSP. Note they run **before** `AudioPassthru` exists (`Main.cpp:67` vs
`:69`) — they only touch `DfxDsp` and the settings store.

### 4.4 `applyConfig` — running instance, exact order

`FxController::applyConfig` (`FxController.cpp:344-602`):

```
 0.  --status           -> printStatus(); RETURN                        :348-352
 1.  --power            -> setPowerState() unless remote session        :367-373
 2.  preset commands, only when model.getPowerState():                  :393-449
        --preset  >  --save_preset  >  --overwrite_preset
                  >  --undo_preset  >  --rename_preset  >  --delete_preset
        (if/else-if chain: strictly first match wins)
 3.  --output           -> setOutputName + exact-name scan + setOutput   :451-462
 4.  --num_bands                                                        :468-473
 5.  --volume_leveling                                                  :476-481
 6.  --balance                                                          :484-489
 7.  --filter_q                                                         :492-497
 8.  --master_gain                                                      :500-505
 9.  --view             -> settings + view_ + showView()                 :507-516
10.  --language         -> setLanguage()                                 :518-521
11.  --run_minimized ? (settings.run_minimized = true; hideMainWindow())
                      : showMainWindow()                                 :523-531
12.  --set_band_freq    (+ main_window_->update() if Pro view)           :536-553
13.  --set_band_gain    (+ update if Pro)                                :555-572
14.  --set_effect       (+ update if Pro)                                :574-600
```

Two behaviours the port must not lose:

- **Step 11 is an `else`.** `fxsound --power=1` on a running instance *also pops the
  window to the front*. Only `--status` escapes it (via the early return at step 0).
- **Steps 12–14 run after step 4**, so `--num_bands=31 --set_band_gain="30:6"` works in a
  single invocation — the size guard at `:558` sees the new band count.

### 4.5 Preset-command exclusivity and name sanitising

`--preset`, `--save_preset`, `--overwrite_preset`, `--undo_preset`, `--rename_preset`,
`--delete_preset` are **mutually exclusive**, processed in exactly that priority order;
extras are silently ignored (`docs/COMMAND_LINE_OPTIONS.md:40`; the `if/else if` chain at
`FxController.cpp:395-448`).

`sanitizePresetName` (`FxController.cpp:377-391`), applied to `--save_preset` and
`--rename_preset`, in order:

1. `removeCharacters("<>:\"/\\|?*")` — strips `< > : " / \ | ? *` (`:378`).
2. Truncate to **64** characters (`:380-383`).
3. `model.isPresetNameValid(preset_name)` — a case-insensitive collision check against
   existing preset names (`:385`). On collision the whole command is a **no-op**
   (returns `""`).

The doc calls out the order dependency explicitly: `--save_preset="Mu:sic"` is a no-op if
a preset named `Music` already exists, because stripping `:` produces a match
(`docs/COMMAND_LINE_OPTIONS.md:52`).

**Linux:** the strip-set is the Windows filesystem reserved set. Keep it anyway — preset
names become `.fac` filenames (`getAutoSavePresetPath`, `FxController.cpp:805-808`) and
cross-platform preset files are the point. Add `/` and NUL handling (already covered) and
reject `.`/`..`. Keep the 64-char cap and the case-insensitive collision rule.

### 4.6 `--status` output

`printStatus()` (`FxController.cpp:635-700`) builds a JSON document and writes it to
`getStatusFile()` = `<userApplicationDataDirectory>/FxSound/status.json`
(`FxController.cpp:635-638`), creating the parent directory first (`:694`).

Shape (field names are exact; the Go mirror at `fxmcp/internal/fxsound/status.go:20-70`
agrees):

```jsonc
{
  "version": "1.2.14.0",                 // ProjectInfo::versionString   :641
  "power": true,                         //                              :642
  "presets": {
    "built_in":     [ { "name": "General", "modified": false }, … ],   // :644-660
    "user_defined": [ { "name": "…",       "modified": true  }, … ]
  },
  "selected_preset": "General",                                        // :666
  "output_devices": [ "…" ],             // only device.isRealDevice    :668-675
  "selected_output": "…",                // getOutputName()             :676
  "equalizer": {
    "num_bands": 10, "master_gain": 0.0, "volume_leveling": 0.0,
    "filter_q": 1.0, "balance": 0.0,
    "bands": [ { "index": 0, "frequency": 60.0, "gain": 0.0 }, … ]     // :678-693
  },
  "effects": {                           // all values × 10 → 0..10 scale
    "clarity": 0.0,                      // getEffectValue(Fidelity)*10 // :695
    "ambience": 0.0, "surround": 0.0,
    "dynamicboost": 0.0, "bass": 0.0                                   // :696-699
  }
}
```

Note `effects.clarity` is sourced from `FxEffects::Fidelity` — the internal enum name and
the JSON name differ (`FxController.cpp:695`). The `×10` conversions mean the DSP stores
effects on a 0..1 scale while both the CLI and the JSON use 0..10.

The console-print block (`FxController.cpp:686-699`) does
`AttachConsole(ATTACH_PARENT_PROCESS)` → `freopen_s(CONOUT$)` → `std::cout` →
`FreeConsole()`, with a comment stating it attaches to the **running instance's** original
console, not the forwarding process's.

**Linux:** write the same JSON to `$XDG_RUNTIME_DIR/fxsound/status.json` (atomically:
write `status.json.tmp` then `rename(2)`), **and** return it in the socket reply so
`fxsound --status` prints it on the caller's stdout — which is what every user and script
actually expects. Keep the field names byte-identical so an eventual `fxmcp` port needs no
schema change.

### 4.7 Linux CLI recommendations

- Parse with `clap` (derive), accepting **both** `--power=1` and `--power 1`. The
  `=`-only restriction is a Windows tokenising artefact
  (`docs/COMMAND_LINE_OPTIONS.md:7`), not a design decision; keeping it would be
  user-hostile on a shell.
- Keep every option spelling **exactly** as in §4.2 (including the `_` separators:
  `--num_bands`, `--filter_q`, `--master_gain`, `--volume_leveling`, `--set_band_freq`,
  `--set_band_gain`, `--set_effect`, `--run_minimized`) so existing scripts and the MCP
  server port keep working. Add `-`-spelled aliases (`--num-bands`, …) as hidden aliases.
- **Change the silent-reset behaviour to an error on the CLI path**, exactly as the Go
  client already does (`ValidateRange`, `ValidateNumEqBands`,
  `fxmcp/internal/fxsound/config.go:186-202`). Silently turning `--filter_q=9` into `1.0`
  is a bug generator. Keep silent clamping only for values read back from the config file.
- Add what Windows lacks and the hotkey story now needs (§2.7):
  `--power=toggle`, `--toggle-window`, `--show`, `--hide`, `--next-preset`,
  `--prev-preset`, `--next-output`, `--quit`.
- `--status` should also support `--status --json`/`--status --pretty` and exit non-zero
  when no instance is running (Windows silently no-ops — `fxmcp/internal/fxsound/running.go:41-49`).
- `$XDG_RUNTIME_DIR` may be unset (cron, ssh without pam_systemd): fall back to
  `/tmp/fxsound-$UID` created 0700 with `O_NOFOLLOW`.

---

## 5. System tray

### 5.1 Registration and identity

`addIcon()` (`FxSystemTrayView.cpp:172-214`):

| Field | Value | Line |
|---|---|---|
| `uFlags` | `NIF_ICON \| NIF_TIP \| NIF_MESSAGE \| NIF_SHOWTIP \| NIF_GUID` | `:202` |
| `guidItem` | `{A8E96325-5269-443C-A0D8-0D02562FE553}` | `:203`, decl `:24-25` |
| `uCallbackMessage` | `WMAPP_FXTRAYICON = WM_APP + 1` (0x8001) | `:204`, `FxSystemTrayView.h:50` |
| `hWnd` | the hidden component window | `:205` |
| `szTip` (initial) | `"FxSound"` | `:206` |
| add / version | `Shell_NotifyIcon(NIM_ADD)` then `uVersion = NOTIFYICON_VERSION_4` + `NIM_SETVERSION` | `:207-211` |
| then | `setVisible(true)` | `:213` |

The icon is re-added whenever Explorer restarts: the class registers the
`"TaskbarCreated"` broadcast message (`:42`) and calls `addIcon()` again on receipt
(`:438-441`). Removal uses `NIM_DELETE` with only `NIF_GUID` set (`:56-59`).

Using a **GUID** rather than a `uID` means Windows remembers the user's show/hide
preference for this icon across reinstalls **but ties it to the exe path** — a
well-known Win32 footgun, and one reason `Shell_NotifyIconGetRect` (§5.7) can fail.

### 5.2 Icon states — complete state machine

Two independent booleans select one of four icons. The same logic appears twice, in
`addIcon()` (`:179-200`) and `setStatus(power, processing)` (`:90-111`), and again for the
**window** icon in `FxMainWindow::setIcon` (`FxMainWindow.cpp:367-399`).

```
power == false                                   -> IDI_LOGO_GRAY    (#6f6f6f bars)
power == true  && processing == false            -> IDI_LOGO_WHITE   (#ffffff bars)
power == true  && processing == true && Dark     -> IDI_LOGO_RED     (#ea3564 / #e63462)
power == true  && processing == true && Light    -> IDI_LOGO_BLUE    (#23b6eb / #23B6EB)
```

`FxThemeMode` is `{ Dark = 0, Light, NumModes }` (`FxTheme.h:28`); default is `Dark`
(`FxTheme.cpp:60` — `FxThemeMode FxTheme::theme_mode_ = FxThemeMode::Dark;`).
If `LoadIcon` returns NULL, `setStatus` returns **without updating the tooltip either**
(`FxSystemTrayView.cpp:113-116`).

Who calls `setStatus`:

| Caller | When | Line |
|---|---|---|
| `FxController::init` | end of startup, `setStatus(power, false)` | `FxController.cpp:785` |
| `FxController::setPowerState` | every power change (and the remote-session forced-off path) | `FxController.cpp:1016`, `:1029` |
| `FxController::timerCallback` | when the 5-tick processing hysteresis flips either way | `FxController.cpp:2075`, `:2085` |
| `FxController::setThemeMode` | theme switch (icon colour depends on theme) | `FxController.cpp:2773` |

`processing` is `audio_process_on_`, which becomes true after **5 consecutive 100 ms
ticks** where `dfx_dsp_.getTotalAudioProcessedTime()` advanced, and false after 5
consecutive ticks where it did not (`FxController.cpp:2061-2088`).

### 5.3 Tooltip

`setStatus` composes the tooltip into a `wchar_t[1024]` (`FxSystemTrayView.cpp:78-84`):

```
<TRANS("FxSound is %s.") with %s = TRANS("on") | TRANS("off")>
<blank line>                       <- literal "\n\n"
<TRANS("Output: ")><selected output device friendly name>
```

Rendered in English:

```
FxSound is on.

Output: Speakers (Realtek High Definition Audio)
```

Sources: format string `:79`, `on`/`off` `:76`, `"\n\n"` `:80`, `"Output: "` `:81`,
device name from `FxModel::getSelectedOutput().deviceFriendlyName` `:83`.

The initial tooltip before the first `setStatus` is just `"FxSound"` (`:206`).

> ⚠️ `swprintf_s` is fed a **translated** format string (`:79`). A bad translation that
> drops or duplicates `%s` is a memory-safety bug on Windows. In Rust use a named
> placeholder + `strfmt`-style runtime formatting, or validate the catalogue at build time.

### 5.4 Mouse and shell interaction

`wndProc` (`FxSystemTrayView.cpp:434-478`) handles `WMAPP_FXTRAYICON` and switches on
`LOWORD(lParam)` (NOTIFYICON_VERSION_4 packing):

| Notification | Behaviour | Line |
|---|---|---|
| `NIN_SELECT` (left click / Enter) | **toggle**: if `isMainWindowVisible()` → `hideMainWindow()`, else `showMainWindow()` | `:446-455` |
| `NIN_BALLOONTIMEOUT`, `NIN_BALLOONUSERCLICK` | re-arm the tooltip: `Shell_NotifyIcon(NIM_MODIFY)` with `NIF_SHOWTIP \| NIF_GUID` | `:457-465` |
| `WM_CONTEXTMENU` (right click / menu key) | `showContextMenu()` | `:467-469` |
| `taskbar_created_message_` | `addIcon()` | `:438-441` |
| anything else | `CallWindowProc(componentWndProc_, …)` | `:472-475` |

There is **no double-click handler** and **no middle-click handler**.

Before the menu is shown the app calls `SetFocus(hWnd)` and `SetForegroundWindow(hWnd)`
(`:326-327`) — the classic Win32 dance to make a tray popup dismiss correctly on
click-away.

### 5.5 The complete tray menu tree

Built in `showContextMenu()` (`FxSystemTrayView.cpp:216-330`). Item ids from
`FxSystemTrayView.h:43-49`: `MENU_ID_OPEN=1`, `MENU_ID_POWER=2`, `MENU_ID_SETTINGS=3`,
`MENU_ID_DONATE=4`, `MENU_ID_EXIT=5`, `PRESET_MENU_ID_START=101`, `OUTPUT_MENU_ID_START=201`.

```
┌─ tray context menu ────────────────────────────────────────────────────────┐
│ Open                                     id 1   -> showMainWindow()        │  :292,298-299,311
│ "Turn On" | "Turn Off"                   id 2   -> setPowerState(!power)   │  :293,300-303,312
│      (text = power ? "Turn Off" : "Turn On";  DISABLED if remote session)  │
│ ┌ Preset Select  ▸  ── only present when power is ON ───────────────────┐  │  :313-316
│ │   <AppPreset 1>                        id 101                          │  │  :228-251
│ │   <AppPreset 2>                        id 102                          │  │
│ │   ──────────── separator inserted where preset.type changes ─────────  │  │  :238-242
│ │   <UserPreset 1>                       id 10n                          │  │
│ │   • name is "<name> *" when preset.modified                            │  │  :231
│ │   • the selected preset is ticked                                      │  │  :233-236
│ └────────────────────────────────────────────────────────────────────────┘  │
│                                                                             │
│ ── if outputs <= 5: inline, preceded by a separator and a section header ─  │  :344-347
│ ┃ (section header) "Playback Device Select"                                 │
│ ┃   <device 1>                           id 201  (ticked if selected)       │  :350-372
│ ┃   <device 2>                           id 202  (DISABLED if channels<2)   │  :356-359
│ ┃ ──────────── trailing separator ────────────────────────────────────────  │  :380
│ ── if outputs >  5: a submenu instead ───────────────────────────────────── │  :339-341,374-377
│ ┌ Playback Device Select ▸  <same items> ──────────────────────────────┐   │
│ └──────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│ Settings                                 id 3   -> modal FxSettingsDialog   │  :294,304-305,318
│ ┌ Theme ▸ ────────────────────────────────────────────────────────────┐    │  :289-290,319
│ │   Dark    (ticked when current mode == Dark)                         │    │
│ │   Light   (ticked when current mode == Light)                        │    │
│ └──────────────────────────────────────────────────────────────────────┘    │
│ Always On Top   (ticked from main_window->isAlwaysOnTop())                  │  :320
│ Donate                                   id 4   -> PayPal URL               │  :295,306-307,321
│ Exit                                     id 5   -> FxController::exit()     │  :296,308-309,322
└─────────────────────────────────────────────────────────────────────────────┘
```

Exact action bindings:

| Item | Action | Line |
|---|---|---|
| Open | `FxController::showMainWindow()` | `:253-255` |
| Turn On/Off | `setPowerState(!FxModel::getModel().getPowerState())` | `:257-259` |
| Preset *n* | `setPreset(id - 101)` | `:244-247` |
| Device *n* | `setOutput(id - 201)` | `:365-368` |
| Settings | construct `FxSettingsDialog`, `runModalLoop()`, then `FxController::refreshOutputList()` | `:261-265` |
| Theme ▸ Dark | `setThemeMode(FxThemeMode::Dark)` | `:267-269` |
| Theme ▸ Light | `setThemeMode(FxThemeMode::Light)` | `:271-273` |
| Always On Top | `setAlwaysOnTop(!isAlwaysOnTop())` | `:275-278` |
| Donate | `URL("https://www.paypal.com/donate/?hosted_button_id=JVNQGYXCQ2GPG").launchInDefaultBrowser()` | `:280-283` |
| Exit | `FxController::exit()` | `:285-287` |

`FxController::checkDeviceChanges()` is called **before** building the menu (`:223`) so the
device list is fresh at popup time.

For comparison, the in-window hamburger menu (`FxMainWindow::showMenu`) is a *different*
tree — Settings, Save New Preset ▸, Overwrite Existing Preset, Undo Preset Changes,
Rename Preset ▸, Delete Preset, Export Presets, Import Presets, Download Bonus Presets,
Check for updates, Theme ▸ {Dark, Light}, Always On Top, Donate — and has **no Exit and no
Open** (`FxMainWindow.cpp`, menu assembly). Theme, Always On Top and Donate are the three
items duplicated in both menus; keep them in sync in the port.

### 5.6 Menu construction rules worth restating

1. **Preset submenu only when power is on** (`:313-316`). With power off the menu is
   Open / Turn On / devices / Settings / Theme / Always On Top / Donate / Exit.
2. **Output list layout depends on count**: `> 5` ⇒ submenu; `<= 5` ⇒ inline, wrapped in a
   separator + a `addSectionHeader(TRANS("Playback Device Select"))` above and a separator
   below (`:339-348, 374-381`).
3. **Device labels are truncated to 30 characters** by `getTruncatedText(name, 30)`
   (`:353`, impl `:422-432`): if longer, drop `(len - 30) + 3` trailing chars and append
   `"..."`, giving a result of exactly 30 characters.
4. **Devices with fewer than 2 channels are shown but disabled**
   (`deviceNumChannel < 2` → `setEnabled(false)`, `:356-359`).
5. **Power item disabled in a remote session** (`power.setEnabled(!SysInfo::isRemoteSession())`, `:303`).
6. **Always On Top's tick comes from the window, not the controller**
   (`FxController::getInstance().getMainWindow()->isAlwaysOnTop()`, `:320`) — a subtle
   difference from `FxController::isAlwaysOnTop()` (`FxController.cpp:2777-2780`) that
   matters if the two ever drift.
7. **Settings blocks.** `settings_dialog.runModalLoop()` (`:263`) spins a nested modal
   loop inside the tray callback. In the Rust port the tray thread must **not** block:
   post a `ShowSettings` event to the UI thread and return immediately.

### 5.7 Tray anchor geometry (used to position the toast and the Lite window)

`getSystemTrayWindowPosition(width, height)` (`FxSystemTrayView.cpp:123-170`):

1. `Shell_NotifyIconGetRect(&{cbSize, hWnd, guidItem})` → the icon's **physical** screen
   rect; on failure returns `{0,0}` (`:132-139`).
2. Convert physical → logical via `Desktop::getDisplays().physicalToLogical()` (`:144-145`).
3. Against the primary display's `userArea` (work area), pick a corner:

```
                 userArea
   ┌───────────────────────────────────────┐
   │(x+10, y+10)                           │   icon left of centreX -> x = area.x + 10
   │                                       │   icon right of centreX-> x = area.right - w - 10
   │                  ·centre              │   icon above centreY   -> y = area.y + 10
   │                                       │   icon below centreY   -> y = area.bottom - h - 10
   │            (right-w-10, bottom-h-10)  │
   └───────────────────────────────────────┘
```
`:147-167`. The margin is **10 px** on every edge (`:151,155,161,165`).

Consumers: the toast (`FxSystemTrayView.cpp:401-402`) and the **Lite view window**
placement (`FxMainWindow.cpp:275-277`).

**Linux:** there is no way to learn where your StatusNotifierItem is drawn — the panel
owns that, across any number of panels, and SNI has no "get my icon rect" call. Do not try.
Replacements:
- For toasts: use the notification daemon (§6.5); the daemon decides placement.
- For the Lite window: position with the compositor. Under Wayland a normal xdg-toplevel
  **cannot set its own position at all**. Either (a) accept compositor placement and
  document a Hyprland rule, or (b) use the `wlr-layer-shell-v1` protocol
  (`smithay-client-toolkit`, layer `Top`, anchor `Top|Right`, margin **10** to match) for
  the Lite view, which is genuinely the right primitive for a corner-anchored panel widget.
  Option (b) is a real dependency decision — flag it (see Open questions).

### 5.8 Linux tray: StatusNotifierItem, and why XEmbed is not an option

**Use the `ksni` crate** (`ksni = "0.3"`), which implements
`org.kde.StatusNotifierItem` plus the `com.canonical.dbusmenu` menu export over `zbus`.

Why not the legacy X11 system tray (XEmbed / `_NET_SYSTEM_TRAY_S<n>`, the protocol behind
`libappindicator`'s X fallback and Qt's `QSystemTrayIcon` on X11):

1. XEmbed works by the panel **reparenting the client's X window** into the tray container.
   Wayland has no cross-client window embedding and no reparenting primitive at all — a
   Wayland client cannot hand a surface to another client's surface tree.
2. Under XWayland the mechanism would need *both* the app and the panel to be X11 clients.
   Modern Wayland panels (waybar, Hyprland's bars, Plasma 6, GNOME Shell) are Wayland
   clients and implement **only** the D-Bus StatusNotifierItem protocol; there is no
   `_NET_SYSTEM_TRAY_S0` selection owner to talk to.
3. SNI is also strictly better for our needs: it carries a structured **menu model**
   (DBusMenu) instead of requiring us to draw and position a popup ourselves, which is
   exactly what `showContextMenu()` (`:216-330`) does by hand today.

Registration flow to implement (ksni does most of it):
`org.kde.StatusNotifierWatcher.RegisterStatusNotifierItem` on the session bus; if the
watcher name is absent, retry with backoff and keep running headless — this is the exact
analogue of the `"TaskbarCreated"` re-registration at `FxSystemTrayView.cpp:438-441`.
`ksni` handles watcher restarts; make sure the port re-registers rather than giving up.

Mapping the Windows tray surface to SNI:

| Windows | SNI / ksni |
|---|---|
| `NIF_ICON` + 4 `HICON`s | `Tray::icon_name()` returning one of `com.fxsound.FxSound-{off,on,processing}`; or `icon_pixmap()` with ARGB32 data for panels with no theme lookup |
| dark/light icon variants | do **not** key off *our* theme. Ship a `-symbolic` monochrome variant and let the panel recolour it; keep the coloured variant for the "processing" state via `Status::NeedsAttention` + `attention_icon_name()` |
| `NIF_TIP` / `szTip` (§5.3) | `ToolTip { title: "FxSound", description: "FxSound is on.\n\nOutput: …", icon_name, icon_pixmap }` — note many panels render only `title`, so put the important line first |
| `NIN_SELECT` toggle | `Tray::activate(x, y)` → toggle main window |
| `WM_CONTEXTMENU` | `Tray::menu()` → `Vec<MenuItem>`; the panel pops it |
| ticked radio items (presets, devices, theme) | `ksni::menu::RadioGroup` (presets, devices, theme are all radio groups) |
| `Always On Top` check | `ksni::menu::CheckmarkItem` |
| disabled items | `enabled: false` |
| section header `"Playback Device Select"` | DBusMenu has no section header: use a `SubMenu` **always** (drop the ≤5 inline special case — it exists only because Win32 menus are cheap) or a disabled label item |
| `Shell_NotifyIcon(NIM_DELETE)` | drop the `ksni::Handle` / let the service task end |

Concrete ksni notes:
- `ksni` runs its own tokio task; the `Tray` struct must be `Send + 'static`. Keep the
  tray's view of the world in an `Arc<Mutex<TrayState>>` (power, processing, presets,
  devices, theme, always-on-top) and call `handle.update(|t| …)` from the UI thread after
  every model change — i.e. wherever the C++ calls `setStatus()` (§5.2) or would rebuild
  the menu.
- Every menu activation must be forwarded to the UI thread via `EventLoopProxy`; never
  touch egui state from the ksni task.
- **GNOME ships no SNI host by default** — users need the AppIndicator extension. Detect
  that no watcher registered within ~5 s and show a one-time notification explaining it,
  because with no tray and a hidden window the app is otherwise invisible and
  unkillable-by-UI.

Icon install set (all four states, plus symbolic):

```
/usr/share/icons/hicolor/{16,22,24,32,48,64,128,256}x.../apps/com.fxsound.FxSound.png
/usr/share/icons/hicolor/scalable/apps/com.fxsound.FxSound.svg
/usr/share/icons/hicolor/scalable/status/com.fxsound.FxSound-off.svg          (#6f6f6f)
/usr/share/icons/hicolor/scalable/status/com.fxsound.FxSound-on.svg           (#ffffff -> use -symbolic)
/usr/share/icons/hicolor/scalable/status/com.fxsound.FxSound-processing.svg   (#23b6eb)
/usr/share/icons/hicolor/symbolic/status/com.fxsound.FxSound-{off,on,processing}-symbolic.svg
```

Regenerate these from the bar geometry in §1.3 **with a transparent background** — the
Windows ICOs' opaque black square (§1.3) would look broken on a light panel.

---

## 6. Notifications

### 6.1 The model's single message slot

`FxModel::pushMessage(String message, std::pair<String,String> link = {})` overwrites
`message_`/`message_link_` and fires `Event::Notification` (`FxModel.h:170-175`).
`popMessage(out,out)` reads and clears them (`:177-183`). The event enum is
`{ Notification=1, Subscription, PresetSelected, PresetListUpdated, PresetModified,
OutputSelected, OutputListUpdated, OutputError, Other }` (`FxModel.h:31`).

The tray view is a `FxModel::Listener`; its handler is:

```cpp
if (!FxController::getInstance().isNotificationsHidden() && model_event == FxModel::Event::Notification)
    showNotification();
```
`FxSystemTrayView.cpp:64-70`

So there is **exactly one pending message**; a second push before the first is displayed
silently replaces it. `hide_notifications` is a persisted setting
(`FxController.cpp:192, 2314-2323`) surfaced as the **"Hide notifications"** toggle
(`FxSettingsDialog.cpp:339, 399-400, 466`).

### 6.2 Path A — the custom in-app toast (the default)

`custom_notification_` is hard-coded `true` (`FxSystemTrayView.cpp:32`), so the custom
toast is always used; the balloon path (§6.3) is effectively dead code but is specified
here because the link-carrying condition (`link.first.isNotEmpty()`) re-selects it too
(`:392`).

`showNotification()` (`:384-420`):

1. `popMessage(message, link)`; bail if empty (`:388-390`).
2. `SHQueryUserNotificationState(&quns)`; **return without showing anything** unless it
   succeeds *and* `quns == QUNS_ACCEPTS_NOTIFICATIONS` (`:394-398`). This is Windows'
   presentation-mode / full-screen / quiet-hours check.
3. `notification_.setMessage(message, link)` (`:400`).
4. Position via `getSystemTrayWindowPosition(w, h)` (§5.7) and `setBounds` (`:401-402`).
5. `notification_.showMessage()` (`:403`).

`FxNotification` geometry and timing — all constants:

| Constant | Value | Source |
|---|---|---|
| `WIDTH` | **216** | `FxNotification.h:33` |
| `HEIGHT` | **80** | `FxNotification.h:34` |
| `MAX_WIDTH` | **560** | `FxNotification.h:35` |
| `MAX_HEIGHT` | **120** | `FxNotification.h:36` |
| `ICON_WIDTH` × `ICON_HEIGHT` | **79 × 12** | `FxNotification.h:42-43` |
| `AD_WIDTH` × `AD_HEIGHT` | 216 × 36 (unused in this path) | `FxNotification.h:44-45` |
| `TITLE_HEIGHT`, `HYPERLINK_HEIGHT` | 24, 24 | `FxNotification.h:46-47` |
| Logo placement | rect `(15, 10, 79, 12)`, centred inside | `FxNotification.cpp:49-50` |
| Max lines | **3** (`lines` beyond index 2 are dropped) | `FxNotification.cpp:53, 101-106` |
| Font | `theme.getSmallFont().withHeight(17.0f)` | `FxNotification.cpp:80, 166` |
| Label border | `BorderSize<int>(1, 0, 2, 0)` (top 1, bottom 2) | `FxNotification.cpp:57` |
| Horizontal margin used for width fitting | **80** when autohiding, **40** otherwise | `FxNotification.cpp:125` |
| Final size | `width × (line_count * 20 + 60)` | `FxNotification.cpp:144` |
| Width growth | grows to `line_width + margin`, capped at `MAX_WIDTH = 560` | `FxNotification.cpp:127-141` |
| Line layout | `x = 40` (autohide) or `20`; `y = i*20 + 30`; size `(w - 2x) × 20` | `FxNotification.cpp:152-155` |
| Link layout | `x` advanced by the text width of the line it shares; `y = link_line*20 + 30`, height 20 | `FxNotification.cpp:160-173` |
| Corner radius | **16** | `FxNotification.cpp:206, 212` |
| Drop shadow radius | **5** | `FxNotification.cpp:207` |
| Fill colour | `FXCOLOR(DefaultFill)` = `#000000` (Dark) / `#ffffff` (Light) | `FxTheme.cpp:23-29`, index 5 of `FxColor` (`FxTheme.h:29`) |
| Text colour | `defaultText` from the scheme (`#b1b1b1` Dark / `#4e4e4e` Light) | `FxNotification.cpp:55`; `FxTheme.cpp:23-29` index 4 |
| Fade-in | `Desktop::getAnimator().fadeIn(this, 200)` = **200 ms** | `FxNotification.cpp:183, 197` |
| Auto-hide, **no link** | **7000 ms** | `FxNotification.cpp:185-188` |
| Auto-hide, **with link** | **8000 ms** | `FxNotification.cpp:189-192` |
| Re-entrancy rule | if a timer is running with interval exactly `7000`, stop it and take over; otherwise **return and drop the new message** | `FxNotification.cpp:65-75` |
| Hide | `stopTimer(); setVisible(false); removeFromDesktop();` (no fade-out) | `FxNotification.cpp:215-220` |

ASCII of a two-line toast with a link (`width = 216`, `height = 2*20+60 = 100`):

```
 x=0                                                              x=216
 ┌──────────────────────────────────────────────────────────────────┐  y=0
 │  ┌────────────┐                                                  │
 │  │ logo 79×12 │ at (15,10)                                       │
 │  └────────────┘                                                  │
 │      line 0 : x=40, y=30, h=20, w=216-80=136                     │  y=30
 │      line 1 : x=40, y=50, h=20        [link starts after text]   │  y=50
 │                                                                  │
 └──────────────────────────────────────────────────────────────────┘  y=100
   corner radius 16, drop shadow radius 5, fill #000000 (dark theme)
```

### 6.3 Path B — the shell balloon (fallback)

Taken only when `!custom_notification_ && link.first.isEmpty()` (`:392`), i.e. never as
shipped. Parameters (`FxSystemTrayView.cpp:407-417`):

| Field | Value |
|---|---|
| `uFlags` | `NIF_INFO \| NIF_GUID \| NIF_REALTIME` |
| `dwInfoFlags` | `NIIF_NOSOUND \| NIIF_RESPECT_QUIET_TIME` |
| `szInfoTitle` | `"FxSound"` |
| `szInfo` | the message (UTF-16, truncated to the field size − 1) |
| call | `Shell_NotifyIcon(NIM_MODIFY, &nid)` |

`NIIF_NOSOUND` → silent. `NIIF_RESPECT_QUIET_TIME` → suppressed during quiet hours.
`NIF_REALTIME` → drop it rather than queue it if it cannot be shown immediately.

### 6.4 Complete catalogue of notification messages

Every `pushMessage` call site in the tree:

| Message (English `TRANS` key) | Link text → URL | When | Source |
|---|---|---|---|
| `" "` (a single space) | `"Click here to see what's new on this version!"` → `https://www.fxsound.com/changelog` | first run after a version change | `FxController.cpp:719` |
| `"FxSound in system tray\r\nClick FxSound icon to reopen"` | — | first hide-to-tray of the session, **2000 ms after** the hide (`Timer::callAfterDelay(2000, …)`) | `FxController.cpp:920-926` |
| `"Thanks for using FxSound! Would you be\r\ninterested in helping us by taking a quick 4 minute\r\nsurvey so we can make FxSound better?"` | `"Take the survey."` → `https://forms.gle/ATx1ayXDWRaMdiR59` | on `showMainWindow` once `survey_timer` (set to now + **7 days** = `7*24*60*60`) has elapsed; then `survey_displayed = true` | `FxController.cpp:938-960`, timer at `:944` |
| `"Preset: " + <name>` | — | preset changed with `notify == true` **and** power on | `FxController.cpp:1099-1102` |
| `"Output: " + <device>` (optionally `+ "\n" + "Preset: " + <preset>`) | — | output device changed | `FxController.cpp:1120, 1132, 1158` |
| `"Output Disconnected"` | — | selected output vanished | `FxController.cpp:1170` |
| `"Changes to preset %s are saved."` | — | overwrite existing user preset | `FxController.cpp:1221` |
| `"New preset %s is saved."` | — | save-as new preset | `FxController.cpp:1234` |
| `"Reached the limit on new presets."` | — | user preset count hit `max_user_presets` (default/clamped **120**) | `FxController.cpp:1239`; limit at `:194-199` |
| `"Preset %s is deleted."` | — | delete preset | `FxController.cpp:1313` |
| `"Presets are restored to factory defaults"` | — | reset presets | `FxController.cpp:1381` |
| `"FxSound is %s."` (`on`/`off`) | — | power toggled **via the global hotkey** (not via tray/UI) | `FxController.cpp:1933` |

Note `%s` substitution uses `FxController::FormatString`, which is `swprintf_s` into a
`wchar_t[1024]` with a translated format string (`FxController.cpp:2905-2912`) — same
translation-injection hazard as the tooltip (§5.3).

### 6.5 Linux notification design

**Use `org.freedesktop.Notifications` on the session bus** (crate: `zbus` directly, or
`notify-rust` which wraps it — `notify-rust` also gives you the `ActionInvoked` loop).

```
Notify(
  app_name      = "FxSound",
  replaces_id   = <the id returned last time, to reuse one slot>,
  app_icon      = "com.fxsound.FxSound",
  summary       = "FxSound",                    // matches szInfoTitle, :413
  body          = <message, with \r\n -> \n>,   // up to 3 lines, §6.2
  actions       = link.is_some() ? ["default", <link.first>] : [],
  hints         = { "urgency": 0 /*low*/, "suppress-sound": true,
                    "desktop-entry": "com.fxsound.FxSound",
                    "category": "device" },
  expire_timeout = link.is_some() ? 8000 : 7000  // §6.2
)
```

Mapping table:

| Windows behaviour | Linux |
|---|---|
| single message slot (`FxModel.h:170-183`) | keep **one** `replaces_id` so a new message replaces the old one in place, exactly matching the C++ semantics |
| `NIIF_NOSOUND` (`:411`) | hint `"suppress-sound" = true` |
| `NIIF_RESPECT_QUIET_TIME` (`:411`) | urgency `Low` — DND policies suppress Low first |
| `SHQueryUserNotificationState != QUNS_ACCEPTS_NOTIFICATIONS` → skip (`:394-398`) | check the daemon's DND state where exposed (GNOME Shell publishes an `Inhibited` property on `org.freedesktop.Notifications`); otherwise just send with Low urgency and let the daemon decide. **Do not** reimplement full-screen detection. |
| link → `FxHyperlink` inside the toast | notification **action**; on `ActionInvoked` open the URL with `xdg-open` via `std::process::Command` (never shell out through `sh -c`) |
| 7000 / 8000 ms timers (`FxNotification.cpp:187,191`) | `expire_timeout` in ms — but note servers may clamp or ignore it |
| fade-in 200 ms, radius 16, shadow 5 | **not portable** — the daemon owns the look. Drop, unless you take the fallback path below. |
| position at the tray corner + 10 px (§5.7) | not available; the daemon positions |

**Fallback when no notification daemon owns the name** (bare Hyprland with no mako/dunst
running is common): render the toast in-app as a second egui viewport, reproducing the
geometry table in §6.2 exactly (216×(20·lines+60), radius 16, fill `#000000`/`#ffffff`,
200 ms fade, 7000/8000 ms timeout), anchored via `wlr-layer-shell` top-right with a 10 px
margin. Detect the daemon by checking whether `org.freedesktop.Notifications` has an owner
(`DBus.NameHasOwner`) at startup and on `NameOwnerChanged`.

Also: the `"FxSound in system tray\r\nClick FxSound icon to reopen"` message is
**critical on Linux**, more so than on Windows, because a user with no SNI host (GNOME
without the extension) can otherwise lose the app entirely. Keep the 2000 ms delay
(`FxController.cpp:923`) and the once-per-session flag (`minimize_tip_`, set false on
first use at `:922`, initialised true at `:130`), and add the no-tray warning from §5.8.

---

## 7. Startup behaviour, autostart and state restore

### 7.1 `run_minimized` — the "start in tray" setting

`run_minimized` is a **persisted boolean**, not just a CLI flag. It is written in five
places:

| Write | Value | Line |
|---|---|---|
| `initConfig`, `--run_minimized` present | `true` | `FxController.cpp:236-239` |
| `applyConfig`, `--run_minimized` present | `true` (then `hideMainWindow()`) | `:523-527` |
| version-change branch in `init` | `false` (forces the window to show after an update) | `:722` |
| `hideMainWindow()` | `true` — **any** hide, including the window's close button | `:915-918` |
| `showMainWindow()` | `false` — any show | `:933` |

Read once, in `init`: `if (!settings_.getBool("run_minimized")) showMainWindow(); else hideMainWindow();`
(`FxController.cpp:774-781`).

The consequence to preserve: **the app remembers whether it was visible when it was last
running.** Quit with the window hidden ⇒ next launch starts in the tray, with no CLI flag
involved.

`hideMainWindow()` (`:908-927`) = `removeFromDesktop() + setVisible(false)` (only if
currently on the desktop) — i.e. it **destroys the OS window**, it does not minimise it.
`showMainWindow()` (`:929-962`) = `main_window_->show()` (`FxMainWindow.cpp:243-264`:
`setVisible(true)`, `addToDesktop(windowAppearsOnTaskbar)`, `toFront(true)`, then
`IsIconic`→`SW_RESTORE`, `SetForegroundWindow`, `SetWindowPos(HWND_TOP, SWP_NOMOVE|SWP_NOSIZE|SWP_SHOWWINDOW)`).

Separately, the **minimise button** in the title bar really does minimise
(`ShowWindow(hwnd, SW_MINIMIZE)`, `FxMainWindow.cpp:642-648`), and the **close button**
hides to tray (`FxMainWindow.cpp:608-616`). Three distinct behaviours — keep all three.

**Linux implications (important architectural note).**

- `winit`'s `Window::set_visible(false)` is **unsupported on Wayland** — you cannot unmap
  an xdg-toplevel and keep it around. The faithful equivalent of `removeFromDesktop()` is
  therefore to **destroy the window** and recreate it on show, which is what the C++ does
  anyway.
- That means the process must be able to run with **zero windows** (for
  `--run_minimized` and after a hide). `eframe::run_native` assumes a window exists for
  the lifetime of the app, so either:
  - drive `winit` 0.30's `ApplicationHandler` yourself with `egui-winit` + `egui-wgpu`
    (recommended — full control over window creation/destruction and a `ControlFlow::Wait`
    loop that idles at ~0 % CPU while hidden), or
  - keep `eframe` and treat hide as "close the immediate viewport, keep the event loop
    alive", which currently fights the framework.
- Raising the window on show is **not guaranteed on Wayland**: there is no
  `SetForegroundWindow`. Use the `xdg-activation-v1` token when one is available (the tray
  activation gives you one via the compositor), and otherwise accept that the window may
  appear unfocused. Document it; do not hack around it.
- `SW_MINIMIZE` → `Window::set_minimized(true)` (xdg_toplevel.set_minimized). Hyprland
  has no minimise concept and will ignore it; consider mapping the minimise button to
  "hide to tray" on compositors without minimise support, behind a setting.

### 7.2 Launch on system startup

`isLaunchOnStartup()` (`FxController.cpp:2789-2799`): opens
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, queries the value named `FxSound`,
returns `size > 0`.

`setLaunchOnStartup(bool)` (`FxController.cpp:2801-2817`): `GetModuleFileName` into
`wchar_t szPath[MAX_PATH]`, then either
`RegSetValueEx(hkey, L"FxSound", 0, REG_SZ, (BYTE*)szPath, sizeof(szPath))` or
`RegDeleteValue(hkey, L"FxSound")`.

Exposed as the **"Launch on system startup"** toggle in General Preferences
(`FxSettingsDialog.cpp:337, 393-394`).

Note: the installer *also* drops a Startup-folder shortcut (`Installer/fxsound.aip:457`),
so autostart can be on from two independent places, and the toggle only reflects the
registry one. Do not reproduce that split.

(Both registry calls ignore the `RegOpenKeyEx` return value and use `hkey` regardless —
`FxController.cpp:2792-2795, 2806-2816`. Don't copy that.)

**Linux — XDG autostart, the single source of truth:**

```
~/.config/autostart/com.fxsound.FxSound.desktop
```

```ini
[Desktop Entry]
Type=Application
Name=FxSound
Exec=fxsound --run_minimized
Icon=com.fxsound.FxSound
Terminal=false
X-GNOME-Autostart-enabled=true
Hidden=false
```

- `isLaunchOnStartup()` ⇒ the file exists **and** is not disabled
  (`Hidden=true` or `X-GNOME-Autostart-enabled=false` mean "user turned it off in the DE's
  own UI" — treat either as off).
- `setLaunchOnStartup(true)` ⇒ write the file (create `~/.config/autostart` 0700 first);
  `false` ⇒ delete it. Write atomically (tmp + rename).
- Note the `Exec` line adds `--run_minimized`, which the Windows Startup shortcut does
  **not** (`Installer/fxsound.aip:457`). That is a deliberate improvement: a session-start
  app that steals focus is a Linux anti-pattern. If you want byte-for-byte parity, drop the
  flag and let the persisted `run_minimized` decide (§7.1) — but prefer the flag.
- Alternative for systemd-based sessions (offer it, don't require it):
  `~/.config/systemd/user/fxsound.service` with
  `After=graphical-session.target`, `PartOf=graphical-session.target`,
  `WantedBy=graphical-session.target`, `ExecStart=/usr/bin/fxsound --run_minimized`,
  `Restart=on-failure`. This gets you restart-on-crash, which the XDG file does not.
  Do not enable both.
- Flatpak: autostart must go through the `org.freedesktop.portal.Background`
  `RequestBackground` portal (`autostart: true`, `commandline: ["fxsound", "--run_minimized"]`),
  since a sandboxed app cannot write the host's `~/.config/autostart`.

### 7.3 State restored at startup

| Setting key | Type / default | Restored where | Line |
|---|---|---|---|
| `power` | bool, default **`1`** | `setPowerState(settings.power)` in `init` | `Settings.cpp:31`; `FxController.cpp:746` |
| `preset` | string, default **`General`** | `setPreset(name)` in `init` | `Settings.cpp:33`; `FxController.cpp:750-751` |
| `hotkeys` | bool, default **`1`** | ctor, gates `registerHotkeys()` | `Settings.cpp:32`; `FxController.cpp:182-187` |
| `cmd_on_off` / `cmd_open_close` / `cmd_next_preset` / `cmd_previous_preset` / `cmd_change_output` | int, defaults `393297/393285/393281/393306/393303` | `getHotkey` | `Settings.cpp:34-38`; `FxController.cpp:2176-2188` |
| `view` | int, valid `1`(Lite)/`2`(Pro); anything else ⇒ **Pro** | ctor | `FxController.cpp:173-181` |
| `menu_clicked` | bool | ctor → model | `FxController.cpp:188` |
| `always_on_top` | bool | ctor; applied by `showLiteView`/`showProView` | `FxController.cpp:190`; `FxMainWindow.cpp:269,284` |
| `hide_help_tooltips` | bool | ctor | `FxController.cpp:191` |
| `hide_notifications` | bool | ctor; gates all toasts | `FxController.cpp:192`; `FxSystemTrayView.cpp:66` |
| `automatic_updates` | bool, **default `true`** | ctor | `FxController.cpp:193` |
| `max_user_presets` | int, clamped to `[10,120]`, else forced **120** | ctor | `FxController.cpp:194-199` |
| `theme_mode` | int, `0`=Dark `1`=Light, out-of-range ⇒ `0` | `init` | `FxController.cpp:755-770`; enum `FxTheme.h:28` |
| `run_minimized` | bool | `init` — decides show vs hide | `FxController.cpp:774-781` |
| `survey_displayed`, `survey_timer` | bool / unix seconds | `init` + `showMainWindow` | `FxController.cpp:772, 938-960` |
| `version` | string | `init`, drives the "what's new" branch | `FxController.cpp:713-723` |
| `num_bands`, `volume_leveling`, `balance`, `filter_q`, `master_gain` | int / double | `initConfig` steps 7–11 | `FxController.cpp:283-341` |
| `output_device_name` | string | `setOutputName` | `FxController.cpp:2739-2743` |
| `prioritize_new_output` | bool, default `false` | | `FxController.cpp:2745-2753` |
| `device_configs` | JSON blob | | `FxController.cpp:2643-2651` |
| `window_x`, `window_y` | int, default `0` | Pro view placement; `(0,0)` ⇒ centre; off-screen ⇒ centre | `FxController.cpp:2629-2640`; `FxMainWindow.cpp:286-306` |
| `last_update_time` | int (unix seconds) | update throttle | `FxController.cpp:2616-2620` |

Storage: JUCE `PropertiesFile` with `applicationName = "FxSound"`,
`folderName = "FxSound"`, suffix `settings` for user values and `secure` for the secure
store (`Settings.h:29-32`, `Settings.cpp:42-49`). User file:
`%APPDATA%\FxSound\FxSound.settings`. A machine-wide **fallback** property set is loaded
from common settings (`%ProgramData%\FxSound\FxSound.settings`,
`Installer/fxsound.aip:55,81,117`); when that is missing or empty the hard-coded XML at
`Settings.cpp:29-40` is used as the fallback set (`Settings.cpp:54-65`).

The shipped defaults file is only three values (`fxsound/Project/FxSound.settings:3-5`):

```xml
<PROPERTIES>
  <VALUE name="power" val="1"/>
  <VALUE name="hotkeys" val="1"/>
  <VALUE name="preset" val="General"/>
</PROPERTIES>
```

**Linux:** one `settings.toml` under `$XDG_CONFIG_HOME/fxsound/`, layered over
`/etc/fxsound/defaults.toml` then `/usr/share/fxsound/defaults.toml` (same
"user overrides machine defaults" semantics as `setFallbackPropertySet`,
`Settings.cpp:59,64`). Save with an atomic tmp+rename and a debounce — JUCE's
`PropertiesFile` auto-saves, and the C++ writes settings on *every* show/hide
(`:918,933`), which would otherwise mean an fsync per window toggle. Keep the key **names**
identical so a migration importer can read an old `FxSound.settings` XML directly.

---

## 8. Recommended Rust module layout

```
src/
  main.rs              // arg parse (clap) -> single-instance probe -> primary or client
  single_instance.rs   // abstract socket + flock, SO_PEERCRED, JSON frames  (§3.3)
  ipc/
    protocol.rs        // Request/Reply serde types, version field
    server.rs          // accept loop -> EventLoopProxy<UserEvent>
    client.rs          // connect, send, print reply, exit
  cli.rs               // the option table of §4.2; init_config() / apply_config() (§4.3, §4.4)
  app/
    state.rs           // the FxController-equivalent: power, preset, output, eq, effects
    lifecycle.rs       // startup order (§2.1), shutdown (§2.4), SIGTERM/SIGINT
    settings.rs        // TOML store + machine-defaults layering (§7.3)
    status.rs          // status.json writer + stdout reply (§4.6)
  tray/
    mod.rs             // ksni Tray impl, menu model (§5.5), state mirror
    icons.rs           // 4 icon states (§5.2), theme/symbolic selection
  notify/
    mod.rs             // org.freedesktop.Notifications (§6.5)
    fallback.rs        // in-app toast reproducing FxNotification geometry (§6.2)
  session/
    logind.rs          // PrepareForSleep, Lock/Unlock, Active, Remote (§2.6)
    shortcuts.rs       // GlobalShortcuts portal, else docs-only (§2.7)
  ui/                  // egui views (Lite/Pro) -- other specs
  crash.rs             // crash-handler + minidumper + panic hook (§2.5)
```

Crates: `clap`, `serde`/`serde_json`/`toml`, `zbus`, `ksni`, `notify-rust` (or raw zbus),
`rustix` (socket/flock/peercred), `winit` 0.30 + `egui`/`egui-winit`/`egui-wgpu` 0.36,
`smithay-client-toolkit` (only if layer-shell is adopted, §5.7), `crash-handler` +
`minidumper`, `tracing` + `tracing-appender` for the log file, `open` for URLs.

---

## 9. Parity checklist

- [ ] Second invocation never starts a second process; it forwards argv and exits 0.
- [ ] A forwarded command line **without** `--run_minimized` shows and raises the window;
      `--status` does not (§4.4 step 11 vs step 0).
- [ ] `initConfig` and `applyConfig` remain **two different option sets** (§4.2 C/R columns).
- [ ] Preset commands are mutually exclusive in the documented priority order (§4.5).
- [ ] Preset names sanitised: strip `<>:"/\|?*`, truncate to 64, case-insensitive collision ⇒ no-op.
- [ ] Rounding: balance/master_gain → integer; filter_q/volume_leveling → 0.5.
- [ ] Tray icon has 4 states; processing uses a 5-sample / 500 ms hysteresis.
- [ ] Tooltip is exactly `"FxSound is on."` + blank line + `"Output: <device>"`.
- [ ] Left-click on the tray **toggles** the window; right-click opens the menu.
- [ ] Menu: Open, Turn On/Off (disabled when remote), Preset Select (only when power on,
      with a separator between built-in and user presets, `*` suffix when modified,
      radio tick), Playback Device Select (labels truncated to 30 chars, <2-channel
      devices disabled, radio tick), Settings, Theme ▸ {Dark, Light}, Always On Top,
      Donate, Exit — **in that order**.
- [ ] Closing the window hides to tray and persists `run_minimized = true`.
- [ ] Quitting only via tray **Exit**, which autosaves a modified preset first.
- [ ] All 12 notification messages present, single-slot replacement semantics,
      7 s / 8 s timeouts, suppressed entirely when `hide_notifications` is on.
- [ ] "FxSound in system tray" tip fires 2 s after the first hide of a session, once.
- [ ] Autostart toggle creates/removes exactly one file.
- [ ] Logind: mute/deactivate on sleep, restore on resume, DSP off on session deactivate
      without rewriting the `power` setting.

---

## Open questions / risks for the Rust port

1. **Version string mismatch.** `ProjectInfo::versionString` is `1.2.14.0`
   (`fxsound/JuceLibraryCode/JuceHeader.h:48`) but the `.jucer`, the `.rc`, the vcxproj
   defines and the installer all say `1.2.15.0`. Which is the real shipped version? The
   port must pick one; `status.json.version` and the "what's new" first-run branch both
   depend on it (`FxController.cpp:641, 713-723`).

2. **JUCE `ArgumentList` semantics are unverifiable here.** JUCE modules are not in the
   tree. `docs/COMMAND_LINE_OPTIONS.md:7` says `--power 1` (space form) is *not*
   supported, and `fxmcp/internal/fxsound/config.go:20-24` agrees — but JUCE's
   `getValueForOption` is generally documented as accepting the space form too. If the
   doc is wrong, some users' scripts rely on the space form. **Recommendation: accept
   both in Rust.** Risk is zero; the reverse is not.

3. **Hiding a window is unsupported on Wayland.** `winit`'s `set_visible` is a no-op
   there, so hide-to-tray must destroy the window, and the process must survive with zero
   windows. This rules out plain `eframe::run_native` as the top-level driver (§7.1).
   Decide early: hand-rolled `winit` `ApplicationHandler` + `egui-winit`/`egui-wgpu`, or
   accept a framework fight.

4. **Raising the window on Wayland is not guaranteed.** There is no `SetForegroundWindow`
   (`FxMainWindow.cpp:259-262`, `FxSystemTrayView.cpp:326-327`). `xdg-activation-v1`
   helps when the request originates from a user action in another client (the tray),
   but "`fxsound --power=1` pops the window to the front" (§4.4 step 11) may simply not
   focus on some compositors. Is that acceptable, or should the CLI stop raising the
   window by default and gain an explicit `--show`?

5. **Tray-icon geometry is unavailable.** `Shell_NotifyIconGetRect` (§5.7) has no SNI
   equivalent, and it feeds both the toast position **and the Lite view's window
   position** (`FxMainWindow.cpp:275-277`). Decision needed: adopt `wlr-layer-shell`
   for the Lite view (correct, but adds `smithay-client-toolkit` and only works on
   wlroots/KDE-style compositors, not GNOME), or let the compositor place the window and
   ship Hyprland/Sway rules.

6. **GNOME has no SNI host out of the box.** With `run_minimized` true and no tray, the
   app is invisible with no way to bring it back except the CLI. Mitigation is specified
   (§5.8: detect no watcher within ~5 s, notify) but it needs a product decision — e.g.
   refuse to start hidden when no watcher and no notification daemon exist.

7. **Global hotkeys are gone.** All five defaults (Ctrl+Shift+{Q,E,A,Z,W}, §2.7) cannot be
   grabbed by a Wayland client. The `GlobalShortcuts` portal covers KDE and newer
   wlroots portals but not GNOME. The Settings dialog's five hotkey rows need redesign —
   portal request UI, or a documentation page with copyable compositor snippets.

8. **`SysInfo::isRemoteSession()` semantics.** Windows disables the power toggle under
   RDP (`SysInfo.cpp:138-141`, used at `FxSystemTrayView.cpp:303`,
   `FxController.cpp:367, 1008`). Under PipeWire, a remote session (waypipe, xrdp,
   Sunshine) does not necessarily mean audio processing is wrong. Should the Linux port
   keep the gate at all, or only when the *audio sink* is a network sink?

9. **`updater.exe` has no Linux analogue.** `checkUpdates()` spawns `updater.exe /silent`
   at 10:00:00 local, once per 24 h (`FxController.cpp:2104-2115, 2611-2626`), and the
   hamburger menu has "Check for updates" → `updater.exe /checknow`. Drop entirely
   (distro-packaged), or replace with a release-feed check behind `automatic_updates`?

10. **`AudioPassthru` init failure is a hard stop** (`FxController.cpp:704-711`, message
    `"Error in system audio configuration. Unable to run FxSound"`), as is the
    `!dfx_enabled_` device-error dialog (`:729-736`). What is the Linux trigger — PipeWire
    not running? No default sink? Decide, because the Linux failure modes (PipeWire
    restarts, sink hot-unplug) are *recoverable* and should not quit the app the way the
    Windows code does.

11. **Translations are missing from this checkout.** `Resources/Strings/` is empty though
    the `.jucer` lists 30 catalogues. The port needs those files (or a fresh translation
    effort) before any non-English string work; pick a format (`fluent` / `gettext`) that
    can validate placeholders, given the `swprintf_s`-with-translated-format hazard at
    `FxSystemTrayView.cpp:79` and `FxController.cpp:2905-2912`.

12. **`FxSettingsDialog` is modal** (`FxSystemTrayView.cpp:263`, `runModalLoop`). egui has
    no modal loop; the tray callback must not block. Confirm the settings UI can be a
    non-modal viewport (or an in-window page) without behavioural surprises — e.g. the
    `refreshOutputList()` that runs *after* the modal returns (`:264`) now needs to happen
    on settings-close instead.

13. **Two autostart mechanisms on Windows** (Run key + Startup shortcut, §7.2) mean the
    toggle can disagree with reality. The Linux port collapses them into one file — but
    if a distro package or a Flatpak also installs an autostart entry, the same split
    returns. Decide that packaging rule now.

14. **`printStatus`'s console attach is a lie on Linux** and the file-mtime polling the
    Go client does (`fxmcp/internal/fxsound/status.go:95-128`, 2 s budget, 100 ms tick)
    exists only because there was no reply channel. The synchronous reply proposed in
    §3.3/§4.6 is strictly better — but any future port of `fxmcp` must be updated in
    lockstep, or it will keep polling a file that is now merely a courtesy copy.
