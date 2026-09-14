# 05 — Application Controller and Model

**Subsystem:** `FxController` + `FxModel` — the brain of the FxSound desktop app.
**Purpose of this document:** a complete, citation-backed reverse-engineering of the Windows/JUCE
implementation, followed by a concrete Rust 1.98.1 / egui-eframe 0.36.0 / PipeWire re-design.

Every numeric value, string literal, key name and enum ordinal below is cited as `path:line`
against the tree at `/home/blackixxce/Загрузки/fxsound-app-main`.

### Path aliases used in citations

| Alias | Real path |
|---|---|
| `FxController.cpp` / `.h` | `fxsound/Source/GUI/FxController.cpp` / `.h` |
| `FxModel.cpp` / `.h` | `fxsound/Source/GUI/FxModel.cpp` / `.h` |
| `Settings.cpp` / `.h` | `fxsound/Source/Utils/Settings/Settings.cpp` / `.h` |
| `DeviceConfig.cpp` / `.h` | `fxsound/Source/Utils/Settings/DeviceConfig.cpp` / `.h` |
| `SysInfo.cpp` / `.h` | `fxsound/Source/Utils/SysInfo/SysInfo.cpp` / `.h` |
| `Main.cpp` | `fxsound/Source/Main.cpp` |
| `FxView.cpp`, `FxProView.cpp`, `FxMainWindow.cpp`, `FxSystemTrayView.cpp`, `FxSettingsDialog.cpp`, `FxHotkeyLabel.cpp`, `FxAudioControls.cpp/.h`, `FxLanguage.cpp`, `FxNotification.cpp/.h`, `FxOutputPreference.cpp`, `FxPresetNameEditor.cpp`, `FxVisualizer.cpp/.h`, `FxWindow.cpp`, `FxTheme.h` | `fxsound/Source/GUI/<name>` |
| `AudioPassthru.h` | `audiopassthru/include/AudioPassthru.h` |
| `DfxDsp.h` | `dsp/include/DfxDsp.h` |
| `dfxpUniversal.cpp` | `dsp/ptutil/dfxp/dfxpUniversal.cpp` |
| `JuceHeader.h` | `fxsound/JuceLibraryCode/JuceHeader.h` |

Application version string is `"1.2.14.0"` (`JuceHeader.h:48`).

---

## 1. Architectural shape

```
                         ┌───────────────────────────────────────────┐
                         │  JUCE message thread ("GUI thread")       │
                         │                                           │
  CLI / 2nd instance ───▶│  FxSoundApplication                       │
  (Main.cpp:136-139)     │    ├─ initialise()   Main.cpp:49          │
                         │    ├─ anotherInstanceStarted() → applyConfig
                         │    └─ shutdown()     Main.cpp:95          │
                         │                                           │
                         │  FxController  (singleton, Timer 100 ms)  │
                         │    owns: Settings, DfxDsp, AudioPassthru* │
                         │           MessageWindow (HWND)            │
                         │                       │                   │
                         │                       ▼ mutates           │
                         │  FxModel  (singleton, ListenerList)       │
                         │           │ notifyListeners()             │
                         │           └─ MessageManager::callAsync ──┐│
                         │                                          ││
                         │  Listeners: FxMainWindow, FxView(+Pro),  ◀┘│
                         │             FxSystemTrayView,             │
                         │             FxOutputPreferenceListModel   │
                         └────────────┬──────────────────────────────┘
                                      │ AudioPassthruCallback::onSoundDeviceChange
                                      │ (COM / MMNotificationClient thread)
                         ┌────────────┴──────────────────────────────┐
                         │  audiopassthru (WASAPI, virtual driver)   │
                         │      calls DfxDsp::processAudio() on the  │
                         │      real-time render thread              │
                         └───────────────────────────────────────────┘
```

Two singletons, both leaked-on-purpose:

* `FxController::getInstance()` — `static FxController* controller = new FxController(); return *controller;`
  (`FxController.h:60-64`), class derives from `DeletedAtShutdown` (`FxController.h:42`).
* `FxModel::getModel()` — Meyers singleton `static FxModel model;` (`FxModel.h:51-55`).

Both are non-copyable (`FxController.h:67-68`, `FxModel.h:57-58`).

---

## 2. `FxModel` — the observable state

### 2.1 Members

| Member | Type | Initial value | Meaning | Cite |
|---|---|---|---|---|
| `power_state_` | `bool` | `false` | Master on/off. `true` = DSP engaged. | `FxModel.h:192`, `FxModel.cpp:23` |
| `presets_` | `Array<Preset>` | empty | Ordered preset list: all `AppPreset` first, then all `UserPreset`. Index == combobox order. | `FxModel.h:193`, `FxController.cpp:843-875` |
| `output_names_` | `StringArray` | empty | Parallel array of `deviceFriendlyName` for `output_devices_`. Rebuilt in `initOutputs`. | `FxModel.h:194`, `FxModel.cpp:35-41` |
| `selected_preset_` | `int` | `0` | Index into `presets_`. Never bounds-checked on read. | `FxModel.h:195`, `FxModel.cpp:25` |
| `output_disconnected_` | `bool` | **uninitialised** | **Dead member.** Declared, never read or written anywhere in the tree. | `FxModel.h:196` |
| `hotkey_support_` | `bool` | `true` | Whether global hotkeys are enabled. Overwritten at startup from settings. | `FxModel.h:198`, `FxModel.cpp:26`, `FxController.cpp:182-183` |
| `menu_clicked_` | `bool` | **uninitialised** | Has the user ever opened the hamburger menu? Suppresses the one-time help bubble. Set from settings at startup. | `FxModel.h:199`, `FxController.cpp:188`, `FxMainWindow.cpp:590-600` |
| `language_` | `int` | `1` | **Dead member.** `getLanguage()`/`setLanguage(int)` on the model are never called; the real language lives in `FxController::language_` as a string. | `FxModel.h:200`, `FxModel.cpp:27` |
| `debug_logging_` | `bool` | `false` | **Dead member.** Never read. | `FxModel.h:201`, `FxModel.cpp:28` |
| `output_devices_` | `std::vector<SoundDevice>` | empty | The *active* output devices shown in the UI (already filtered and priority-sorted by the controller). | `FxModel.h:202` |
| `selected_output_device_` | `SoundDevice` | `{}` | Currently selected output. Compared by `pwszID`. | `FxModel.h:203`, `FxModel.cpp:30` |
| `message_` | `String` | empty | One-slot notification mailbox (overwrites, does not queue). | `FxModel.h:205` |
| `message_link_` | `std::pair<String,String>` | `{}` | `(link text, URL)` for the notification. | `FxModel.h:206` |
| `listeners_` | `ListenerList<Listener>` | empty | Observers. No locking. | `FxModel.h:208` |

### 2.2 `Preset`

```cpp
struct Preset final {            // FxModel.h:34-40
    String     name;             //  display name, from the .fac file's embedded name
    String     path;             //  absolute path of the .fac file
    PresetType type;             //  AppPreset=1 | UserPreset=2
    bool       modified = false; //  the "*" dirty marker
};
```

`enum PresetType { AppPreset=1, UserPreset=2 };` (`FxModel.h:32`).

### 2.3 Event enum — exact ordinals

```cpp
enum Event { Notification=1, Subscription, PresetSelected, PresetListUpdated,
             PresetModified, OutputSelected, OutputListUpdated, OutputError, Other };
```
(`FxModel.h:31`) — so: `Notification=1, Subscription=2, PresetSelected=3, PresetListUpdated=4,
PresetModified=5, OutputSelected=6, OutputListUpdated=7, OutputError=8, Other=9`.

`Subscription` (2) is **never raised anywhere in the tree** — dead ordinal, but keep the numbering
if you ever need to read old state.

### 2.4 Event dispatch

```cpp
void FxModel::notifyListeners(Event model_event) {       // FxModel.cpp:155-164
    MessageManager::callAsync([this, model_event]() {
        auto& listeners = listeners_.getListeners();
        for (auto i = 0; i < listeners.size(); i++)
            listeners.getUnchecked(i)->modelChanged(model_event);
    });
}
```

Key properties an implementer must preserve:

* **Always asynchronous.** Even when called from the GUI thread, the callback is posted, not
  executed inline. So a caller that raises three events in a row gets three deferred callbacks,
  all observing the *final* state.
* **Always on the message thread,** even when raised from the WASAPI/COM device-notification
  thread via `onSoundDeviceChange` (`FxController.cpp:2119`).
* **No locking** — `listeners_` can be mutated (`addListener`/`removeListener`,
  `FxModel.h:185-186`) concurrently with a pending dispatch. Model fields are read by the
  listener *after* the state change, so a rapid second change can be missed/coalesced.

### 2.5 Who raises which event

| Event | Raised by | Cite |
|---|---|---|
| `Notification` (1) | `pushMessage()` | `FxModel.h:170-175` |
| `Subscription` (2) | — nobody — | |
| `PresetSelected` (3) | `selectPreset(i, notify=true)` | `FxModel.cpp:70-81` |
| `PresetListUpdated` (4) | `initPresets()`, `removePreset()` | `FxModel.cpp:46-52`, `60-68` |
| `PresetModified` (5) | `setPresetModified()` — **only** when the flag actually changes **and** the index equals `selected_preset_` | `FxModel.cpp:128-140` |
| `OutputSelected` (6) | `setSelectedOutput(dev, notify=true)` | `FxModel.h:116-123` |
| `OutputListUpdated` (7) | `initOutputs()` | `FxModel.cpp:33-44` |
| `OutputError` (8) | `notifyOutputError()` — sole caller `FxController::selectProcessingOutput` | `FxModel.h:125-128`, `FxController.cpp:1621` |
| `Other` (9) | `setPowerState()` (default arg) | `FxModel.h:79-83` |

### 2.6 Who reacts to which event

Registered listeners (`addListener` sites): `FxView` (`FxView.cpp:27`, removed `:55`),
`FxMainWindow` (`FxMainWindow.cpp:187`, removed `:240`), `FxSystemTrayView`
(`FxSystemTrayView.cpp:30`, removed `:49`), `FxOutputPreferenceListModel`
(`FxOutputPreference.cpp:205`, removed `:214`).

| Event | Listener | Reaction | Cite |
|---|---|---|---|
| *any* | `FxMainWindow` | `power_button_.setPowerState(model.getPowerState())` — resyncs the power button on **every** event | `FxMainWindow.cpp:603-606` |
| `Notification` | `FxSystemTrayView` | if `!FxController::isNotificationsHidden()` → `showNotification()` → `popMessage()` then either the custom `FxNotification` window or a Shell balloon | `FxSystemTrayView.cpp:64-70`, `384-420` |
| `PresetSelected` | `FxView` | `preset_list_.setSelectedId(selected+1, dontSendNotification)` | `FxView.cpp:144-147` |
| `PresetSelected` | `FxProView` | additionally `update()` — re-reads all effect/EQ values from the DSP into the sliders | `FxProView.cpp:130-138` |
| `PresetListUpdated` | `FxView` | rebuilds the combobox; inserts a separator where `PresetType` changes; item text is `name + " *"` when `modified` | `FxView.cpp:149-169` |
| `PresetListUpdated` | `FxOutputPreferenceListModel` | fires `onModelChanged` callback | `FxOutputPreference.cpp:274-281` |
| `PresetModified` | `FxView` | `changeItemText(sel+1, name [+ " *"])`; if the popup is not open, also `setText(...)` | `FxView.cpp:171-199` |
| `OutputSelected` | `FxView` | `endpoint_list_.setSelectedId(getSelectedOutputIndex()+1)`; shows/hides the error notification | `FxView.cpp:115-127` |
| `OutputListUpdated` | `FxView` | rebuilds the endpoint combobox; items with `deviceNumChannel < 2` are added but **disabled** | `FxView.cpp:97-113` |
| `OutputListUpdated` | `FxOutputPreferenceListModel` | fires `onModelChanged` | `FxOutputPreference.cpp:274-281` |
| `OutputError` | `FxView` | if `!isPlaybackDeviceAvailable()` → disable the selected item and set the combo's error flag; else clear it | `FxView.cpp:129-142` |

### 2.7 Notification mailbox semantics

`pushMessage(msg, link={})` overwrites `message_`/`message_link_` and raises `Notification`
(`FxModel.h:170-175`). `popMessage(out,out)` copies out and clears (`FxModel.h:177-183`).
Because dispatch is async, **two `pushMessage` calls in quick succession lose the first message.**
`FxController::savePreset` works around this with a blocking `Thread::sleep(2000)` on the message
thread (`FxController.cpp:1238`) — a bug to fix, not to port.

---

## 3. `FxController` — compile-time constants

| Constant | Value | Cite |
|---|---|---|
| `NUM_SPECTRUM_BANDS` | `10` | `FxController.h:45` |
| `DEFAULT_NUM_EQ_BANDS` | `10` | `FxController.h:46` |
| `DEFAULT_NORMALIZATION` | `0.0f` | `FxController.h:47` — **dead**, never referenced |
| `DEFAULT_VOLUME_LEVELING` | `0.0f` | `FxController.h:48` |
| `DEFAULT_BALANCE` | `0.0f` | `FxController.h:49` |
| `DEFAULT_FILTER_Q` | `1.0f` | `FxController.h:50` |
| `DEFAULT_MASTER_GAIN` | `0.0f` | `FxController.h:51` |
| `MIN_GAIN` | `-12.0f` (EQ band boost/cut floor) | `FxController.h:52` |
| `MAX_GAIN` | `+12.0f` (EQ band boost/cut ceiling) | `FxController.h:53` |
| `HK_CMD_ON_OFF` | `"cmd_on_off"` | `FxController.h:54` |
| `HK_CMD_OPEN_CLOSE` | `"cmd_open_close"` | `FxController.h:55` |
| `HK_CMD_NEXT_PRESET` | `"cmd_next_preset"` | `FxController.h:56` |
| `HK_CMD_PREVIOUS_PRESET` | `"cmd_previous_preset"` | `FxController.h:57` |
| `HK_CMD_NEXT_OUTPUT` | `"cmd_change_output"` | `FxController.h:58` |
| `CMD_ON_OFF` | `1001` (Win32 hotkey id) | `FxController.h:219` |
| `CMD_OPEN_CLOSE` | `1002` | `FxController.h:220` |
| `CMD_NEXT_PRESET` | `1003` | `FxController.h:221` |
| `CMD_PREVIOUS_PRESET` | `1004` | `FxController.h:222` |
| `CMD_NEXT_OUTPUT` | `1005` | `FxController.h:223` |
| `AUTO_SAVE_INTERVAL` | `600` ticks × 100 ms = 60 s | `FxController.cpp:2102` |

`enum ViewType { Lite = 1, Pro = 2 };` (`FxController.h:40`).
`enum FxThemeMode : int { Dark=0, Light, NumModes };` (`FxTheme.h:28`).
`enum FxEffects::EffectType { Fidelity=0, Ambience=1, Surround=2, DynamicBoost=3, Bass=4, NumEffects=5 };`
(`FxAudioControls.h:32`) — bit-identical to `DfxDsp::Effect` (`DfxDsp.h:38`), so the cast at
`FxController.cpp:1753` is safe.

---

## 4. `FxController` — complete member state

| Member | Type | Initial | Meaning / lifecycle | Cite |
|---|---|---|---|---|
| `message_window_` | `MessageWindow` | ctor-init with name `L"FxSoundHotkeys"` | Hidden HWND used solely to receive `WM_HOTKEY`, `WM_POWERBROADCAST`, `WM_WTSSESSION_CHANGE`. Window class name is `"FXSOUND_" + hex(Time::getHighResolutionTicks())`. | `FxController.h:180-217`, `.cpp:126` |
| `hotkeys_registered_` | `bool` | `false` | Guards double `RegisterHotKey`. | `.h:256`, `.cpp:132` |
| `powerNotify_` | `HPOWERNOTIFY` | `nullptr` | Handle from `RegisterSuspendResumeNotification`, resolved dynamically from `user32.dll`. | `.h:257`, `.cpp:135`, `787-799` |
| `unregister_suspend_resume_notification_` | fn ptr | `nullptr` | Paired unregister, called in dtor. | `.h:258`, `.cpp:136`, `210-214` |
| `main_window_` | `FxMainWindow*` | `nullptr` | Non-owning. | `.h:260`, `.cpp:151` |
| `system_tray_view_` | `FxSystemTrayView*` | (uninit until `init()`) | Non-owning. | `.h:261`, `.cpp:702` |
| `audio_passthru_` | `AudioPassthru*` | `nullptr` | Non-owning; owned by `FxSoundApplication`. | `.h:262`, `.cpp:152`, `Main.cpp:69` |
| `dfx_dsp_` | `DfxDsp` | value member | The DSP engine handle. | `.h:263` |
| `settings_` | `FxSound::Settings` | value member | Persisted settings façade. | `.h:264` |
| `device_count_` | `uint32_t` | `0` | Last observed `sound_devices.size()`; the hot-plug edge detector. | `.h:265`, `.cpp:138` |
| `file_logger_` | `unique_ptr<FileLogger>` | created in ctor | `%APPDATA%\FxSound\fxsound.log`, welcome message `"FxSound logs"`. | `.h:266`, `.cpp:154` |
| `view_` | `ViewType` | from `settings("view")`; if `<=0 or >2` → `Pro` | Lite vs Pro layout. | `.h:267`, `.cpp:173-181` |
| `language_` | `String` | set by `setLanguage()` in `initConfig` | BCP-47-ish code, e.g. `"en"`, `"pt-br"`, `"zh-CN"`. | `.h:268`, `.cpp:2330-2338` |
| `dfx_enabled_` | `bool` | `true` in ctor, recomputed in `initOutputs` | `true` iff a device whose friendly name contains `L"FxSound Audio Enhancer"` is present. | `.h:269`, `.cpp:128`, `1462`, `1504-1507` |
| `authenticated_` | `bool` | `true` | Vestigial licensing flag; only gates the survey prompt. | `.h:270`, `.cpp:129`, `955` |
| `output_changed_` | `bool` | `false` | Set after `setAsPlaybackDevice`; makes the next timer tick sleep 200 ms and skip. | `.h:271`, `.cpp:133`, `1156`, `2054-2059` |
| `playback_device_available_` | `bool` | `true` | Mirrors `AudioPassthru::isPlaybackDeviceAvailable()`; edge triggers `OutputError`. | `.h:272`, `.cpp:134`, `1617-1622` |
| `output_device_name_` | `String` | `L""` → lazily loaded from `settings("output_device_name")` | The *name* (not id) of the chosen output. | `.h:273`, `.cpp:139`, `2729-2743` |
| `active_output_devices_` | `std::vector<SoundDevice>` | empty | Real, active, ≥2-channel devices, priority-sorted. Feeds the UI list. | `.h:274`, `.cpp:1490-1510` |
| `output_devices_` | `std::vector<SoundDevice>` | empty | The *full* device snapshot (`getSoundDevices(false)`), incl. inactive. Used by `isOutputDevicePresent`. | `.h:275`, `.cpp:726`, `2664-2675` |
| `always_on_top_` | `bool` | `settings("always_on_top")`, default `false` | | `.h:276`, `.cpp:190` |
| `hide_help_tooltips_` | `bool` | `settings("hide_help_tooltips")`, default `false` | | `.h:277`, `.cpp:191` |
| `hide_notifications_` | `bool` | `settings("hide_notifications")`, default `false` | | `.h:278`, `.cpp:192` |
| `auto_updates_` | `bool` | `settings("automatic_updates", true)` | | `.h:279`, `.cpp:193` |
| `audio_process_time_` | `unsigned long` | `0` | Last sampled `DfxDsp::getTotalAudioProcessedTime()` (milliseconds). | `.h:281`, `.cpp:141` |
| `audio_process_on_counter_` | `int` | `0` | Consecutive 100 ms ticks where the counter advanced. | `.h:282`, `.cpp:142` |
| `audio_process_off_counter_` | `int` | `0` | Consecutive 100 ms ticks where it did not. | `.h:283`, `.cpp:143` |
| `audio_process_on_` | `bool` | `false` | Debounced "audio is flowing" state. Drives tray icon, logo animation, visualizer. | `.h:284`, `.cpp:144` |
| `audio_process_start_time_` | `std::time_t` | `-1LL` | **Dead member.** Assigned once, never read. | `.h:285`, `.cpp:146` |
| `preset_dirty_` | `bool` | `false` | "There are unsaved DSP changes that the 60 s autosave must flush." | `.h:287`, `.cpp:148` |
| `auto_save_counter_` | `int` | `0` | Tick counter towards `AUTO_SAVE_INTERVAL`. | `.h:288`, `.cpp:149` |
| `minimize_tip_` | `bool` | `true` | One-shot: show the "FxSound in system tray" tip on the first hide of the process. | `.h:290`, `.cpp:130`, `920-926` |
| `survey_tip_` | `bool` | `!settings("survey_displayed")`, set in `init()` | Gate for the one-time survey nag. | `.h:291`, `.cpp:772` |
| `max_user_presets_` | `int` | `settings("max_user_presets")`, clamped to `[10,120]`, else forced to `120` and written back | Hard cap on user presets. | `.h:292`, `.cpp:194-199` |
| `session_id_` | `DWORD` | `0` then `ProcessIdToSessionId(GetCurrentProcessId(), &session_id_)` | Used to ignore device changes belonging to another logon session. | `.h:294`, `.cpp:203-204`, `2121-2122` |
| `lock_` | `CriticalSection` | — | Held for the whole of `onSoundDeviceChange`. | `.h:296`, `.cpp:2124` |
| `save_lock_` | `CriticalSection` | — | Held by `autoSavePreset`, `savePreset`, `renamePreset`. **Not** held by `deletePreset` or `resetPresets`. | `.h:297`, `.cpp:816`, `1206`, `1246` |

---

## 5. Startup sequence

### 5.1 Process entry

`START_JUCE_APPLICATION(FxSoundApplication)` (`Main.cpp:331`), `moreThanOneInstanceAllowed() → false`
(`Main.cpp:46`): a second launch is routed into the running instance's
`anotherInstanceStarted(commandline)` → `FxController::applyConfig()` (`Main.cpp:136-139`).

`initialise()` (`Main.cpp:49-93`) in order:

1. `SetUnhandledExceptionFilter(unhandledExceptionFilter)` — writes
   `%APPDATA%\FxSound\fxsound.dmp` via `MiniDumpWriteDump(..., MiniDumpNormal, ...)` and appends
   a symbolised stack trace to the log (`Main.cpp:145-306`).
2. `CoInitializeEx(0, COINIT_MULTITHREADED)` + `CoInitializeSecurity(RPC_C_AUTHN_LEVEL_DEFAULT,
   RPC_C_IMP_LEVEL_IMPERSONATE, EOAC_NONE)` (`Main.cpp:56-61`).
3. `LookAndFeel::setDefaultLookAndFeel(&theme_)` (`Main.cpp:63`).
4. `setWorkingDirectory()` — `SetCurrentDirectory(dirname(GetModuleFileName(NULL)))`
   (`Main.cpp:65`, `308-321`). **This is what makes the factory-preset path `./Factsoft` work.**
5. `FxController::getInstance().initConfig(commandline)` — constructs the controller (see §5.2)
   and applies CLI/settings defaults.
6. `new AudioPassthru`, `new FxMainWindow`, `new FxSystemTrayView` (`Main.cpp:69-71`).
7. `FxController::init(main_window, system_tray_view, audio_passthru)` (`Main.cpp:73`).

Also exported: `NvOptimusEnablement = 0` and `AmdPowerXpressRequestHighPerformance = 0` so the
app never forces the discrete GPU (`Main.cpp:31-35`).

### 5.2 `FxController::FxController()` — `FxController.cpp:126-206`

1. Field defaults (§4 "Initial" column).
2. `file_logger_ = FileLogger::createDefaultAppLogger(L"FxSound", L"fxsound.log", L"FxSound logs")`
   and immediately logs: `"v" + version`, `SystemStats::getOperatingSystemName()`, and one of
   `"x86"` / `"x64"` / `"ARM64"` from `GetNativeSystemInfo` (`.cpp:154-171`).
3. `view_` from settings, clamped (`.cpp:173-181`).
4. `hotkeys_support = settings("hotkeys") && SysInfo::canSupportHotkeys()`. `canSupportHotkeys()`
   is a hard `return true;` (`SysInfo.cpp:133-136`). Push into the model; if true call
   `registerHotkeys()` (`.cpp:182-187`).
5. `model.setMenuClicked(settings("menu_clicked"))` (`.cpp:188`).
6. `always_on_top_`, `hide_help_tooltips_`, `hide_notifications_`, `auto_updates_`,
   `max_user_presets_` (`.cpp:190-199`).
7. `SetWindowLongPtr(hwnd, GWLP_USERDATA, this)` so the static WndProc can find the instance
   (`.cpp:201`).
8. `ProcessIdToSessionId(...)` + `WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)`
   (`.cpp:203-205`).

### 5.3 `FxController::init()` — `FxController.cpp:696-801`

Guarded by `if (!isTimerRunning())` — the whole body is skipped if the 100 ms timer is already up.

1. `audio_passthru_->init()`; on non-zero → modal `AlertWindow` with
   `"Error in system audio configuration. Unable to run FxSound"` then `systemRequestedQuit()`
   (`.cpp:704-711`).
2. **Version-change migration**: if `settings("version") != ProjectInfo::versionString`:
   * `RegDeleteTree(HKEY_CURRENT_USER, L"Software\\DFX")` — nukes the legacy DFX registry tree.
   * `pushMessage(" ", { "Click here to see what's new on this version!",
     "https://www.fxsound.com/changelog" })`.
   * write `version`, write `run_minimized = false`.
   (`.cpp:713-723`)
3. `audio_passthru_->setDspProcessingModule(&dfx_dsp_)` (`.cpp:725`).
4. `output_devices_ = getSoundDevices(false)`; `initOutputs(output_devices_)` (`.cpp:726-727`).
5. If `!dfx_enabled_ && !isRemoteSession()` → remove main window from desktop, show the modal
   `FxDeviceErrorMessage` (400×142 px, links to
   `https://www.fxsound.com/learning-center/installation-troubleshooting` and
   `https://www.fxsound.com/support`), then quit (`.cpp:729-736`, message window geometry
   `.cpp:82-85`).
6. `audio_passthru_->registerCallback(this)` (`.cpp:738`).
7. `mkdir %APPDATA%\FxSound\Presets` if missing (`.cpp:740-744`).
8. `setPowerState(settings("power"))` (`.cpp:746`).
9. `initPresets()` (`.cpp:748`).
10. `setPreset(settings("preset"))` (`.cpp:750-751`).
11. `showView()` (`.cpp:753`).
12. Theme: `theme_mode = settings("theme_mode", 0)`, clamped to `[0, NumModes)`; if it differs from
    the current mode, apply it, `sendLookAndFeelChange()`, `theme->loadFont(language_)`
    (`.cpp:755-770`).
13. `survey_tip_ = !settings("survey_displayed")` (`.cpp:772`).
14. `showMainWindow()` or `hideMainWindow()` per `settings("run_minimized")` (`.cpp:774-781`).
15. Sync icons: `main_window_->setIcon(power, false)`, `system_tray_view_->setStatus(power, false)`
    (`.cpp:783-785`).
16. Dynamically bind `RegisterSuspendResumeNotification` / `UnregisterSuspendResumeNotification`
    from `user32.dll` and register with `DEVICE_NOTIFY_WINDOW_HANDLE` (`.cpp:787-799`).

### 5.4 Shutdown

* `FxSoundApplication::shutdown()` → `autoSaveModifiedPreset()`, `stopTimer()`,
  destroy `AudioPassthru`, `UiaDisconnectAllProviders()`, destroy tray then window, clear
  localised strings and LookAndFeel, `CoUninitialize()` (`Main.cpp:95-125`).
* `~FxController()` → unregister suspend/resume notification, `stopTimer()`,
  `WTSUnRegisterSessionNotification`, `unregisterHotkeys()` (`.cpp:208-218`).
* `FxController::exit()` → `autoSaveModifiedPreset()` then `systemRequestedQuit()`
  (`.cpp:998-1005`); bound to the tray "Exit" item (`FxSystemTrayView.cpp:285-287`).

---

## 6. Command-line interface

Parsed with `juce::ArgumentList` in two places. `initConfig` runs in the *first* instance before
the UI exists; `applyConfig` runs in the *already-running* instance when a second process is
launched.

### 6.1 `initConfig(commandline)` — `FxController.cpp:221-342`

| Option | Effect | Validation | Cite |
|---|---|---|---|
| `--run_minimized` | `settings("run_minimized") = true` | — | `.cpp:236-239` |
| `--power <0\|1>` | `settings("power") = (int != 0)` | any non-zero = on | `.cpp:241-247` |
| `--preset <name>` | `settings("preset") = name` (unquoted) | — | `.cpp:249-252` |
| `--output <name>` | `setOutputName(name)` → writes `output_device_name` | — | `.cpp:254-257` |
| `--view <1\|2>` | `settings("view")`, `view_` | must equal `Lite(1)` or `Pro(2)` | `.cpp:259-267` |
| `--language <code>` | `setLanguage(code)` | falls back to `settings("language")`, then `SystemStats::getDisplayLanguage()`, then `"en"` | `.cpp:269-278`, `2330-2335` |
| `--num_bands <n>` | `setNumEqBands(n)` | must be one of `{5,10,15,20,31}` else `10` | `.cpp:283-293` |
| `--volume_leveling <dB>` | `setVolumeLeveling(v)` | `v<0 \|\| v>4` → `0.0` | `.cpp:295-305` |
| `--balance <dB>` | `setBalance(v)` | `v<-20 \|\| v>+20` → `0.0` | `.cpp:307-317` |
| `--filter_q <x>` | `setFilterQ(v)` | `v<1 \|\| v>3` → `1.0` | `.cpp:319-329` |
| `--master_gain <dB>` | `setMasterGain(v)` | `v<-20 \|\| v>+20` → `0.0` | `.cpp:331-341` |

When an option is absent, the corresponding `settings_` value is used as the source instead
(`.cpp:286, 298, 310, 322, 334`) — so the same clamps also sanitise the persisted state.

### 6.2 `applyConfig(commandline)` — `FxController.cpp:344-602`

`--status` short-circuits: `printStatus(); return;` (`.cpp:348-352`).

`--power` is honoured **only when `!SysInfo::isRemoteSession()`** (`.cpp:367-373`).

The preset-mutating options form a single `else if` chain guarded by `model.getPowerState()`
(`.cpp:393-449`) — **only the first match runs**:

| Option | Precondition | Action | Cite |
|---|---|---|---|
| `--preset <name>` | name non-empty | `setPreset(name)` | `.cpp:395-401` |
| `--save_preset <name>` | sanitised name non-empty **and** `isPresetModified()` **and** `getUserPresetCount() < max_user_presets_` | `savePreset(name)` | `.cpp:402-412` |
| `--overwrite_preset` | `isPresetModified()` **and** selected is `UserPreset` | `savePreset()` | `.cpp:413-420` |
| `--undo_preset` | `isPresetModified()` | `undoPreset()` | `.cpp:421-427` |
| `--rename_preset <name>` | sanitised name non-empty **and** `!isPresetModified()` **and** selected is `UserPreset` | `renamePreset(name)` | `.cpp:428-440` |
| `--delete_preset` | selected is `UserPreset` | `deletePreset()` | `.cpp:441-448` |

**Preset-name sanitiser** (`.cpp:377-391`): strip the characters `<>:"/\|?*`, truncate to
**64** characters, require non-empty and `model.isPresetNameValid(name)` (case-insensitive
uniqueness, `FxModel.cpp:142-153`). The interactive editor enforces the same rules
(`FxPresetNameEditor.cpp:9-17` reserved chars, `:52` `setInputRestrictions(64)`).

Remaining options:

| Option | Effect | Cite |
|---|---|---|
| `--output <name>` | `setOutputName(name)`, then scan `getSoundDevices()` for a matching `deviceFriendlyName` and `setOutput(pwszID)` | `.cpp:451-462` |
| `--num_bands`, `--volume_leveling`, `--balance`, `--filter_q`, `--master_gain` | same clamps as §6.1, applied only when present | `.cpp:464-505` |
| `--view <1\|2>` | persist + `showView()` | `.cpp:507-516` |
| `--language <code>` | `setLanguage()` | `.cpp:518-521` |
| `--run_minimized` | persist + `hideMainWindow()`; **otherwise `showMainWindow()`** | `.cpp:523-531` |
| `--set_band_freq "i:hz[,i:hz…]"` | pairs split on `,` then `:`; the whole list is ignored if `pairs.size() > getNumEqBands()`; each `setEqBandFrequency(i, hz)`; then `main_window_->update()` in Pro view | `.cpp:536-553` |
| `--set_band_gain "i:dB[,…]"` | same shape; `setEqBandBoostCut(i, dB)` | `.cpp:555-572` |
| `--set_effect "name:v[,…]"` | at most **5** pairs; names (lower-cased): `fidelity`\|`clarity`, `ambience`, `surround`, `dynamicboost`\|`dynamic_boost`, `bass`\|`bassboost`\|`bass_boost` | `.cpp:574-601` |

### 6.3 `--status` output — `printStatus()` — `FxController.cpp:604-694`

Writes `%APPDATA%\FxSound\status.json` (`.cpp:604-607`, `676-678`) and, best-effort, echoes it to
the *original* console via `AttachConsole(ATTACH_PARENT_PROCESS)` + `freopen_s("CONOUT$")`
(`.cpp:684-693`).

```jsonc
{
  "version": "1.2.14.0",
  "power": true,
  "presets": {
    "built_in":     [ { "name": "...", "modified": false } ],
    "user_defined": [ { "name": "...", "modified": true  } ]
  },
  "selected_preset": "General",
  "output_devices": [ "…friendly names of isRealDevice==true devices…" ],
  "selected_output": "…",
  "equalizer": {
    "num_bands": 10, "master_gain": 0.0, "volume_leveling": 0.0,
    "filter_q": 1.0, "balance": 0.0,
    "bands": [ { "index": 0, "frequency": 30.0, "gain": 0.0 } ]
  },
  "effects": {                      // NOTE: DSP values are 0..1, multiplied by 10 here
    "clarity": 0.0, "ambience": 0.0, "surround": 0.0,
    "dynamicboost": 0.0, "bass": 0.0
  }
}
```
(field order and the `*10.0f` scaling: `.cpp:613-672`)

---

## 7. Settings persistence

### 7.1 Storage backends

`FxSound::Settings` (`Settings.cpp:26-66`) constructs **three** `ApplicationProperties`:

| Object | `filenameSuffix` | `doNotSave` | Purpose | Cite |
|---|---|---|---|---|
| `app_secure_properties_` | `"secure"` (`SECURE_EXTN`) | `false` | **Dead.** Storage parameters are set but `getUserSettings()` is never called, so no `.secure` file is ever created or read. | `Settings.cpp:44-46`, `.h:32` |
| `app_user_properties_` | `"settings"` (`SETTINGS_EXTN`) | `false` | The real read/write store. `user_settings_ = getUserSettings()`. | `Settings.cpp:48-50` |
| `app_default_properties_` | `"settings"` | `true` | Read-only machine-wide fallback: `getCommonSettings(false)`. | `Settings.cpp:52-54` |

Fallback chain: if the common-settings file is missing or empty, an XML literal is parsed into
`default_settings_`; otherwise the common settings file *replaces* those built-in defaults
wholesale. Either way `user_settings_->setFallbackPropertySet(&default_settings_)`
(`Settings.cpp:55-65`).

`applicationName = "FxSound"`, `folderName = "FxSound"` (`Settings.h:29-30`, `Settings.cpp:42-43`).
With JUCE 6.1.6's `PropertiesFile::Options::getDefaultFile()` on Windows this resolves to:

* user store: `%APPDATA%\FxSound\FxSound.settings`
* common store: `%ProgramData%\FxSound\FxSound.settings`

(The JUCE source is not vendored in this tree — this path composition is JUCE's documented
Windows behaviour, not a line in this repo. Verify before relying on it for migration.)

### 7.2 Built-in default property set — verbatim

```xml
<PROPERTIES>                                     <!-- Settings.cpp:29-40 -->
    <VALUE name="power"               val="1"/>
    <VALUE name="hotkeys"             val="1"/>
    <VALUE name="preset"              val="General"/>
    <VALUE name="cmd_on_off"          val="393297"/>
    <VALUE name="cmd_open_close"      val="393285"/>
    <VALUE name="cmd_next_preset"     val="393281"/>
    <VALUE name="cmd_previous_preset" val="393306"/>
    <VALUE name="cmd_change_output"   val="393303"/>
</PROPERTIES>
```

Only these **eight** keys have declared defaults. Every other key falls back to JUCE's
zero/empty default, or to the explicit `default_value` argument of
`getInt`/`getBool` (`Settings.h:41-43`).

### 7.3 Complete settings key inventory

| Key | Type | Default | Read at | Written at |
|---|---|---|---|---|
| `power` | bool | `1` (true) | `.cpp:746`, `2036`, `2044` | `.cpp:244`, `246`, `1026` |
| `hotkeys` | bool | `1` (true) | `.cpp:182` | `.cpp:2164` |
| `preset` | string | `"General"` | `.cpp:750`, `1451` | `.cpp:251`, `1082` |
| `cmd_on_off` | int | `393297` = `0x00060051` → Ctrl+Shift+**Q** | `.cpp:2178` | `.cpp:2252`, `2258`, `2263` |
| `cmd_open_close` | int | `393285` = `0x00060045` → Ctrl+Shift+**E** | idem | idem |
| `cmd_next_preset` | int | `393281` = `0x00060041` → Ctrl+Shift+**A** | idem | idem |
| `cmd_previous_preset` | int | `393306` = `0x0006005A` → Ctrl+Shift+**Z** | idem | idem |
| `cmd_change_output` | int | `393303` = `0x00060057` → Ctrl+Shift+**W** | idem | idem |
| `view` | int | `0` → invalid → `Pro(2)` | `.cpp:173` | `.cpp:264`, `512`, `896`, `902` |
| `menu_clicked` | bool | `false` | `.cpp:188` | `.cpp:977` |
| `always_on_top` | bool | `false` | `.cpp:190` | `.cpp:2785` |
| `hide_help_tooltips` | bool | `false` | `.cpp:191` | `.cpp:2310` |
| `hide_notifications` | bool | `false` | `.cpp:192` | `.cpp:2322` |
| `automatic_updates` | bool | **`true`** (explicit arg) | `.cpp:193` | `.cpp:2609` |
| `max_user_presets` | int | `0` → out of `[10,120]` → forced to `120` | `.cpp:194` | `.cpp:197` |
| `run_minimized` | bool | `false` | `.cpp:774` | `.cpp:238`, `525`, `722`, `917`, `933` |
| `language` | string | `""` → OS display language → `"en"` | `.cpp:271` | `.cpp:2338` |
| `num_bands` | int | `0` → invalid → `10` | `.cpp:286` | `.cpp:1781` |
| `volume_leveling` | double | `0.0` | `.cpp:298` | `.cpp:1793` |
| `balance` | double | `0.0` | `.cpp:310` | `.cpp:1805` |
| `filter_q` | double | `0.0` → `<1` → `1.0` | `.cpp:322` | `.cpp:1829` |
| `master_gain` | double | `0.0` | `.cpp:334` | `.cpp:1817` |
| `version` | string | `""` | `.cpp:714` | `.cpp:721` |
| `theme_mode` | int | `0` = `Dark` | `.cpp:755` | `.cpp:2768` |
| `survey_displayed` | bool | `false` | `.cpp:772` | `.cpp:953` |
| `survey_timer` | int (unix secs) | `0` | `.cpp:941` | `.cpp:945` |
| `device_configs_version` | int | `0`; current schema version is **`2`** | `.cpp:1467` | `.cpp:1471`, `1480` |
| `device_configs` | JSON-in-string | `""` | `DeviceConfig.cpp:172` | `DeviceConfig.cpp:196` |
| `output_device_name` | string | `""` | `.cpp:2733` | `.cpp:2742` |
| `prioritize_new_output` | bool | `false` | `.cpp:2747`, `DeviceConfig.cpp:57` | `.cpp:2752` |
| `last_update_time` | int (unix secs) | `0` | `.cpp:2617` | `.cpp:2621` |
| `window_x` | int | `0` | `.cpp:2637` | `.cpp:2631` |
| `window_y` | int | `0` | `.cpp:2638` | `.cpp:2632` |

`window_x`/`window_y` are written from `FxMainWindow::moved()` **only when the whole window is
inside the total desktop bounds and the view is Pro** (`FxMainWindow.cpp:618-630`).

### 7.4 Non-settings persistence: the registry

| Registry operation | Purpose | Cite |
|---|---|---|
| `RegDeleteTree(HKEY_CURRENT_USER, "Software\\DFX")` | one-shot legacy cleanup on version change | `.cpp:717` |
| `RegQueryValueEx(HKCU\Software\Microsoft\Windows\CurrentVersion\Run, "FxSound")` → `size > 0` | read "launch on startup" | `.cpp:2789-2799` |
| `RegSetValueEx(..., "FxSound", REG_SZ, GetModuleFileName(NULL))` / `RegDeleteValue` | write "launch on startup" | `.cpp:2801-2818` |

Note `RegSetValueEx` passes `sizeof(szPath)` — the full `MAX_PATH*2` bytes, not the string
length — so trailing garbage is written. Harmless on Windows, but don't copy the pattern.

### 7.5 `device_configs` JSON schema

Stored as a JSON **string** under one settings key (`Settings::setJson` →
`setValue(key, JSON::toString(json))`, `Settings.cpp:150-153`).

```jsonc
[                                              // DeviceConfig.cpp:125-134, 186-197
  { "device_id":          "{0.0.0.00000000}.{guid}",  // SoundDevice::pwszID
    "device_name":        "Speakers (Realtek…)",      // SoundDevice::deviceFriendlyName — the key
    "preset":             "Rock",                     // "" = no per-device preset
    "device_form_factor": "Speakers" }                // SoundDevice::deviceFormFactor
]
```

**Array order *is* the priority order** — index 0 is highest priority
(`compareOutputDevicePriority`, `.cpp:2700-2720`: a device not present in the list gets priority
`device_configs.size()`, i.e. lowest; the return value is `priority1 - priority2`).

`loadDeviceConfigs` de-duplicates by `device_name`, keeping the first occurrence
(`DeviceConfig.cpp:151-166, 183`).

`initDeviceConfigs` (`DeviceConfig.cpp:26-52`) seeds the list by sorting the device vector
**twice with `std::sort` (not stable)**: first by `isActive` descending, then by
`(isDefaultDevice || isTargetedRealPlaybackDevice)` descending — the second sort can scramble the
first key's ordering among equals. Then it keeps only `isRealDevice` entries.

`updateDeviceConfigs` (`DeviceConfig.cpp:54-108`) appends any device (matched by **name**) not
already in the list — at the **front** if `prioritize_new_output`, else at the back — saves, fires
`onDeviceConfigsUpdate` and returns. If nothing was added, it fires `onDeviceConfigsUpdate` once
per config whose device is no longer present (a loop that can fire many times).

---

## 8. Preset lifecycle

### 8.1 Filesystem layout

| Kind | Path | Cite |
|---|---|---|
| Factory ("app") presets | `<exe dir>\Factsoft\*.fac` | `.cpp:844-846` + `Main.cpp:308-321` |
| User presets | `%APPDATA%\FxSound\Presets\*.fac` | `.cpp:856-858`, created `.cpp:740-744` |
| Auto-saved (dirty) presets | `%APPDATA%\FxSound\AutoSave\<name>.fac` | `.cpp:803-812` |
| Export target | `%USERPROFILE%\Documents\FxSound\Presets\Export\<name>.fac` | `.cpp:1386` |
| Status dump | `%APPDATA%\FxSound\status.json` | `.cpp:604-607` |
| Log | `%APPDATA%\FxSound\fxsound.log` | `.cpp:154` |
| Crash minidump | `%APPDATA%\FxSound\fxsound.dmp` | `Main.cpp:149-155` |

### 8.2 `initPresets()` — `FxController.cpp:841-876`

1. Enumerate `<cwd>/Factsoft/*.fac`, non-recursive; for each, `dfx_dsp_.getPresetInfo(path)`; if
   `preset_info.name` non-empty, push `{name, path, AppPreset}`.
2. Enumerate `%APPDATA%\FxSound\Presets\*.fac` the same way, pushing `UserPreset`.
3. For every preset, `modified = File(getAutoSavePresetPath(name)).existsAsFile()`.
4. `model.initPresets(presets)` → `PresetListUpdated`.

**Identity is the embedded name inside the `.fac`, not the filename.** The autosave path is
derived from `name`, so two presets with the same name in different folders collide.

### 8.3 `setPreset(int selected_index, bool notify = true)` — `FxController.cpp:1048-1105`

```
if index out of [0, presetCount)            → return false
if index != model.selected && model.isPresetModified(model.selected)
                                            → autoSavePreset(model.selected)     // flush the old one
if preset.path is non-empty:
    if AutoSave/<name>.fac exists:
        dfx_dsp_.loadPreset(autosave_path)
        model.setPresetModified(index, true)          // see note below
    else:
        dfx_dsp_.loadPreset(preset.path)
        model.setPresetModified(index, false)
    settings("preset") = preset.name
    model.selectPreset(index, true)                   // → PresetSelected
    for e in 0..NumEffects:                           // force-reapply, 0..1 → 0..10 round-trip
        dfx_dsp_.setEffectValue(e, dfx_dsp_.getEffectValue(e) * 10)
    for b in 0..getNumEqBands():
        dfx_dsp_.setEqBandFrequency(b, dfx_dsp_.getEqBandFrequency(b))
        dfx_dsp_.setEqBandBoostCut (b, dfx_dsp_.getEqBandBoostCut(b))
if notify && model.getPowerState()
                                            → pushMessage("Preset: " + name)
return true
```

Three subtleties:

1. **`setPresetModified(index, …)` at line 1074/1079 runs *before* `selectPreset`.** Because
   `FxModel::setPresetModified` only raises `PresetModified` when `preset_index ==
   selected_preset_` (`FxModel.cpp:136-139`), the event does **not** fire when switching to a
   different preset. The UI still shows the `*` because the combobox item text already carries it
   from the `PresetListUpdated` rebuild (`FxView.cpp:164`).
2. **The re-apply loops call `dfx_dsp_` directly, not `FxController::setEffectValue`.** That is
   deliberate: going through the controller would set `preset_dirty_` and mark the preset modified.
3. `setPreset(const String& name, bool notify)` (`.cpp:1032-1046`) is a linear name search over the
   model, `==` (case-**sensitive**), returning `false` if not found.

### 8.4 Dirty marking

Three entry points set both flags, and only on the 0→1 edge of the model flag:

| Function | Guard | Cite |
|---|---|---|
| `setEffectValue(effect, v)` | rejects `v < 0 \|\| v > 10`; then `dfx_dsp_.setEffectValue`; `if (!model.isPresetModified()) model.setPresetModified(sel, true)`; `preset_dirty_ = true` | `.cpp:1756-1771` |
| `setEqBandFrequency(band, hz)` | `band < getNumEqBands()`, `hz` inside `getEqBandFrequencyRange(band)` | `.cpp:1849-1869` |
| `setEqBandBoostCut(band, dB)` | `band < getNumEqBands()`, `MIN_GAIN(-12) ≤ dB ≤ MAX_GAIN(+12)` | `.cpp:1896-1914` |

**The five "global" controls do NOT mark a preset dirty** — `setNumEqBands`, `setVolumeLeveling`,
`setBalance`, `setMasterGain`, `setFilterQ` only push to the DSP and write a settings key
(`.cpp:1778-1830`). They are app-wide, not per-preset.

Rounding applied on the way in:

| Setter | Rounding | Persisted key | Cite |
|---|---|---|---|
| `setVolumeLeveling(dB)` | `round(dB*2)/2` (0.5 steps) | `volume_leveling` | `.cpp:1789-1794` |
| `setBalance(dB)` | `round(dB)` (1.0 steps) | `balance` | `.cpp:1801-1806` |
| `setMasterGain(dB)` | `round(dB)` | `master_gain` | `.cpp:1813-1818` |
| `setFilterQ(x)` | `round(x*2)/2` | `filter_q` | `.cpp:1825-1830` |
| `setNumEqBands(n)` | none | `num_bands` | `.cpp:1778-1782` |

UI slider ranges (`min, max, step`): master gain `(-20, 20, 2)` (`FxAudioControls.cpp:312`),
volume leveling `(0, 4, 0.5)` (`:330`), filter Q `(1, 3, 0.5)` (`:348`), balance `(-20, 20, 2)`
(`FxBalanceSlider.cpp:36`), EQ band boost `(-12, +12, 1.0)` (`FxEqualizer.cpp:48`), EQ band centre
frequency `(min, max, (max-min)/100)` per band (`FxEqualizer.cpp:55`).
Band-count choices: `{5, 10, 15, 20, 31}` (`FxAudioControls.h:106`).

### 8.5 Autosave

```cpp
String getAutoSavePath()                 // "%APPDATA%\FxSound\AutoSave"          .cpp:803-807
String getAutoSavePresetPath(name)       // getAutoSavePath() + "\\" + name + ".fac"  .cpp:809-812

void autoSavePreset(int idx) {           // .cpp:814-829   ScopedLock(save_lock_)
    if (preset.name.isEmpty()) return;
    mkdir(getAutoSavePath());
    dfx_dsp_.savePreset(preset.name, getAutoSavePath());
    preset_dirty_ = false;  auto_save_counter_ = 0;
}
void deleteAutoSavedPreset(name) {       // .cpp:831-839  (no lock!)
    if (file exists) file.deleteFile();
    preset_dirty_ = false;  auto_save_counter_ = 0;
}
```

Triggers: the 60 s timer (`.cpp:2103-2110`), switching away from a modified preset
(`.cpp:1061-1065`), `autoSaveModifiedPreset()` on exit (`.cpp:991-996`, called from
`FxController::exit` `.cpp:1000` and `FxSoundApplication::shutdown` `Main.cpp:99`).

### 8.6 Save / rename / delete / undo / reset

| Operation | Preconditions (enforced by UI at `FxMainWindow.cpp:536-540`) | Behaviour | Cite |
|---|---|---|---|
| **Overwrite** `savePreset("")` | `isPresetModified() && type==UserPreset && power` | `dfx_dsp_.savePreset(preset.name, userPresetsDir)`; `deleteAutoSavedPreset(name)`; `setPresetModified(idx,false)`; message `"Changes to preset %s are saved."` | `.cpp:1204-1222` |
| **Save New** `savePreset(name)` | `isPresetModified() && userPresetCount < max_user_presets_ && power` | save under the new name into the user dir; `deleteAutoSavedPreset(old name)`; `initPresets()`; `setPreset(newName)`; message `"New preset %s is saved."`; if count now == max → **`Thread::sleep(2000)`** then `"Reached the limit on new presets."` | `.cpp:1223-1241` |
| **Rename** `renamePreset(new)` | `!isPresetModified() && type==UserPreset && power`; no-op if `new == old` | save under `new`; `SHFileOperation(FO_DELETE, old_path, FOF_NOCONFIRMATION\|FOF_NOERRORUI\|FOF_SILENT)` — **permanent, no `FOF_ALLOWUNDO`**; `initPresets()`; `setPreset(new)`; `deleteAutoSavedPreset(old)`; clear modified | `.cpp:1244-1276` |
| **Delete** `deletePreset()` | `type==UserPreset && power` | `deleteAutoSavedPreset(name)`; `SHFileOperation` delete; `initPresets()`; select the output's configured preset if any, else index `0`; message `"Preset %s is deleted."` | `.cpp:1278-1315` |
| **Undo** `undoPreset()` | `isPresetModified()` | clear modified → `PresetModified`; `deleteAutoSavedPreset(name)`; `setPreset(idx)` (now loads the original) | `.cpp:1317-1332` |
| **Reset to factory** `resetPresets()` | UI enables it when `userPresetCount > 0` or any preset is modified (`FxSettingsDialog.cpp:210-220`) | reset the five globals to their `DEFAULT_*`; delete every autosave of a modified preset; `SHFileOperation`-delete **every** `UserPreset` file; `initPresets()`; select the output's configured preset else `0`; message `"Presets are restored to factory defaults"` | `.cpp:1334-1382` |
| **Export** `exportPresets(list)` | menu enabled when `!isPresetModified() && power` | `mkdir Documents\FxSound\Presets\Export`; per preset, if the target exists ask `"Preset file %s already exists in the export path, do you want to overwrite the preset file?"`; else `dfx_dsp_.exportPreset(src, name, dir)` | `.cpp:1384-1417` |
| **Import** `importPresets(files, out imported, out skipped)` | same | for each file, `getPresetInfo()`; if the name is unique, copy into the user preset dir as `<name>.fac` and record in `imported`, else record in `skipped`; if anything was imported → `initPresets()` + `setPreset(settings("preset"))`, return `true` | `.cpp:1419-1458` |

⚠ `savePreset("")` writes into the **user** preset directory using the *current* preset's name even
if the current preset is an `AppPreset` — the only thing preventing a shadow copy of a factory
preset is the caller-side guard. The Rust port should enforce the invariant inside the function.

### 8.7 Preset state machine

States of the *selected* preset, `M` = `model.isPresetModified(sel)`, `A` = autosave file exists,
`D` = `preset_dirty_`.

| # | State | `M` | `A` | `D` | Meaning |
|---|---|---|---|---|---|
| S0 | Clean | 0 | 0 | 0 | Loaded from its canonical `.fac`; no `*`. |
| S1 | Dirty, not yet flushed | 1 | 0 | 1 | User moved a slider; `*` shown; autosave pending. |
| S2 | Dirty, flushed | 1 | 1 | 0 | 60 s tick (or preset switch / app exit) wrote the autosave. |
| S3 | Dirty, re-touched | 1 | 1 | 1 | Further edits after a flush. |

| From | Event | To | Side effects |
|---|---|---|---|
| S0 | `setEffectValue` / `setEqBand*` | S1 | `setPresetModified(sel,true)` → `PresetModified`; `preset_dirty_=true` (`.cpp:1766-1770`) |
| S1 | 600-tick autosave, or switch-away, or exit | S2 | write `AutoSave/<name>.fac`; `preset_dirty_=false`; `auto_save_counter_=0` (`.cpp:2103-2110`, `1061-1065`) |
| S2 | another edit | S3 | `preset_dirty_=true` only (`M` already 1, so **no** `PresetModified` event) |
| S3 | autosave | S2 | rewrite the autosave file |
| S1/S2/S3 | `undoPreset()` | S0 | clear `M` (→ `PresetModified`), delete autosave, reload from `path` (`.cpp:1317-1332`) |
| S1/S2/S3 | `savePreset("")` | S0 | write user `.fac`, delete autosave, clear `M`, message (`.cpp:1213-1222`) |
| S1/S2/S3 | `savePreset(name)` | S0 (on the **new** preset) | write new `.fac`, delete the *old* autosave, `initPresets`, select new (`.cpp:1223-1241`) |
| any | app restart | S0 or S2 | `initPresets` re-derives `M` purely from autosave-file existence (`.cpp:869-873`); `setPreset` then loads the autosave (`.cpp:1071-1075`) |

---

## 9. Audio output device selection and hot-plug

### 9.1 `SoundDevice` (the model the controller reasons over) — `AudioPassthru.h:32-53`

| Field | Type | Meaning in controller logic |
|---|---|---|
| `pwszID` | `std::wstring` | Stable device id — the equality key everywhere. |
| `deviceFriendlyName` | `std::wstring` | Display name **and** the `DeviceConfig` key. |
| `deviceFormFactor` | `std::wstring` | e.g. `"HDMI"`, `"Speakers"` — stored in `DeviceConfig`. |
| `deviceNumChannel` | `int` | Devices with `< 2` are excluded from `active_output_devices_` and disabled in the UI combobox. |
| `isRealDevice` | `bool` | False for the FxSound virtual device. |
| `isActive` | `bool` | Endpoint currently present/enabled. |
| `isDefaultDevice` | `bool` | Windows default render endpoint. |
| `isTargetedRealPlaybackDevice` | `bool` | The device the FxSound virtual driver is currently piping into. |

### 9.2 `initOutputs(sound_devices)` — cold start — `FxController.cpp:1460-1538`

1. `dfx_enabled_ = false`, clear `active_output_devices_`.
2. Schema migration: if `settings("device_configs_version") != 2` → `initDeviceConfigs()` and write
   `2`. Else load; if the loaded list is empty → `initDeviceConfigs()` + write `2`; else
   `updateDeviceConfigs()` (`.cpp:1467-1487`).
3. Partition: for each device —
   * `isRealDevice && isActive && deviceNumChannel >= 2` → push into `active_output_devices_`;
     if it is `isDefaultDevice || isTargetedRealPlaybackDevice`, remember it as `default_output`.
   * `!isRealDevice && name.find(L"FxSound Audio Enhancer") != npos` → `dfx_enabled_ = true`.
   (`.cpp:1490-1508`)
4. `sortByDeviceConfigPriority(active_output_devices_)` — `std::stable_sort` by
   `compareOutputDevicePriority` (`.cpp:1510`, `1714-1725`).
5. Choose the output:
   * if `getOutputName()` is empty and we found a `default_output` → adopt its name;
   * else if there is at least one active device → look for one whose name equals
     `getOutputName()`; if found use it, else `default_output = getPreferredOutput()` and adopt
     that name.
   (`.cpp:1512-1534`)
6. `model.initOutputs(active_output_devices_)` → `OutputListUpdated`; then
   `setOutput(default_output.pwszID)` (`.cpp:1536-1537`).

`getPreferredOutput()` walks `device_configs` **in priority order** and returns the first
connected device matching by name; falls back to `active_output_devices_[0]`, else `{}`
(`.cpp:2677-2698`).

### 9.3 `setOutput(const String id, bool notify = true)` — `FxController.cpp:1107-1182`

```
for each device in getSoundDevices():
    if !isRealDevice: continue
    if device.pwszID != id: continue
    found = true
    message = "Output: " + friendlyName
    if getOutputName() != friendlyName:                       // the output is actually changing
        cfg = DeviceConfig::getDeviceConfig(settings, friendlyName)
        if cfg.preset non-empty:
            setPreset(cfg.preset, notify=false)
            if power: message += "\nPreset: " + cfg.preset
    model.setSelectedOutput(device, notify)                   // → OutputSelected when notify
    setOutputName(friendlyName)                               // persists output_device_name
    if !isTimerRunning():                                     // FxSound is OFF
        if device.isDefaultDevice: break                      //   already the system default → done
    else:                                                     // FxSound is ON
        if device.isTargetedRealPlaybackDevice: break          //   driver already targets it → done
    audio_passthru_->setAsPlaybackDevice(device)
    output_changed_ = true
    pushMessage(message)
    break

if !found:
    audio_passthru_->mute(true); powerOn(false); pushMessage("Output Disconnected")
else if power:
    powerOn(true); audio_passthru_->mute(false)

system_tray_view_->setStatus(power, isAudioProcessing())
```

`setOutput(int index)` maps a combobox index through `model.getOutputDevices()` (`.cpp:1184-1192`).

### 9.4 Hot-plug entry point — `onSoundDeviceChange(bool processing)` — `FxController.cpp:2119-2143`

Called by `audiopassthru` (an `AudioPassthruCallback`, `AudioPassthru.h:55-59`) from a COM
notification thread.

```
if (session_id_ != WTSGetActiveConsoleSessionId()) return;   // another user's session → ignore
ScopedLock auto_lock(lock_);
if (isTimerRunning()) {                 // FxSound processing is ON
    if (processing) {
        output_devices_ = getSoundDevices(false);
        DeviceConfig::updateDeviceConfigs(settings_, output_devices_);
        selectProcessingOutput(getSoundDevices(true));
    }
} else {                                // FxSound is OFF
    output_devices_ = getSoundDevices(false);
    DeviceConfig::updateDeviceConfigs(settings_, output_devices_);
    syncOutputWithSystemDefault(getSoundDevices(true));
}
```

Note the asymmetry: when the timer is running but `processing == false`, **nothing happens at all**.

### 9.5 `selectProcessingOutput(devices)` — FxSound ON — `FxController.cpp:1615-1663`

1. `available = audio_passthru_->isPlaybackDeviceAvailable()`; if it differs from
   `playback_device_available_`, update it and raise `OutputError` (`.cpp:1617-1622`).
2. If `devices.size() != device_count_` (a device was added or removed):
   * `updateOutputs(devices)`; `device_count_ = devices.size()`;
   * if `!dfx_enabled_` → `stopTimer()`, and unless in a remote session, show the modal
     `FxDeviceErrorMessage` and quit (`.cpp:1625-1642`).
3. Else (count unchanged) — look for a real device with `isTargetedRealPlaybackDevice` whose name
   differs from `getOutputName()`; if found, adopt its name, `model.setSelectedOutput(dev)`
   (→ `OutputSelected`), and apply the device's configured preset with `notify=false`
   (`.cpp:1643-1662`).

### 9.6 `updateOutputs(devices)` — `FxController.cpp:1540-1612`

1. Snapshot `prev_active_devices`; rebuild `active_output_devices_` from
   `isActive && isRealDevice && deviceNumChannel >= 2`; `sortByDeviceConfigPriority`.
2. If `devices.size() > device_count_` (net addition): load `device_configs` and find the first
   **newly appeared** real ≥2-channel device (not in `prev_active_devices` by `pwszID`) that has a
   *higher* priority than the current output (`compareOutputDevicePriority(new, current) < 0`);
   that becomes `preferred_device` (`.cpp:1558-1584`).
3. If no such device: keep the current output if it is still in the active list; otherwise
   `getPreferredOutput()` (`.cpp:1586-1605`).
4. `model.initOutputs(active_output_devices_)` → `OutputListUpdated`; `setOutput(preferred.pwszID)`
   — called **unconditionally**, because even an unchanged output has to refresh the UI
   (`.cpp:1607-1611`).

### 9.7 `syncOutputWithSystemDefault(devices)` — FxSound OFF — `FxController.cpp:1666-1712`

Rebuilds `active_output_devices_` from `isRealDevice && deviceNumChannel >= 2` (note: **no
`isActive` filter here** — an inconsistency with §9.6), sorts by priority; if the device count
changed, republish the list to the model and update `device_count_`. Then it follows the *system*
default: the first `isDefaultDevice` entry becomes the selected output (plus its configured
preset, `notify=false`). If there is no default device at all, `setOutput(getPreferredOutput())`.

### 9.8 Other device helpers

| Function | Semantics | Cite |
|---|---|---|
| `checkDeviceChanges()` | forwards to `audio_passthru_->checkDeviceChanges()`; called when the tray context menu opens (`FxSystemTrayView.cpp:223`) | `.cpp:1199-1202` |
| `isOutputDeviceConnected(name)` | name is in `active_output_devices_` | `.cpp:2651-2662` |
| `isOutputDevicePresent(name)` | name is in the full `output_devices_` snapshot and `isRealDevice` | `.cpp:2664-2675` |
| `refreshOutputList()` | re-sort + `initOutputs` + `setSelectedOutput(current)` → `OutputListUpdated` + `OutputSelected`. Called after the settings dialog closes (`FxMainWindow.cpp:454`, `FxSystemTrayView.cpp:264`). | `.cpp:2722-2727` |
| `getDeviceConfigs()` / `saveDeviceConfigs()` | thin wrappers over `DeviceConfig` with key `"device_configs"` | `.cpp:2641-2649` |

---

## 10. Power state and the processing pipeline

### 10.1 `setPowerState(bool)` — `FxController.cpp:1007-1030`

```
if (!dfx_enabled_ || SysInfo::isRemoteSession()):
    main_window_->enablePowerButton(false)          // tooltip: "Audio enhancements are not
    model.setPowerState(false)                      //  available over Remote Desktop"
    powerOn(false)                                  //  (FxMainWindow.cpp:413)
    tray.setStatus(false,false); window.setIcon(false,false)
    return                                          // NOTE: settings("power") is NOT written
main_window_->enablePowerButton(true)
model.setPowerState(power_state)                    // → Event::Other
powerOn(power_state)
settings("power") = power_state
tray.setStatus(power_state, audio_process_on_)
window.setIcon(power_state, audio_process_on_)
```

### 10.2 `powerOn(bool)` — `FxController.cpp:1727-1749`

| `on` | Actions |
|---|---|
| `true` | `dfx_dsp_.powerOn(true)`; if the timer is not running, `startTimer(100)` |
| `false` | `dfx_dsp_.powerOn(false)`; `stopTimer()` if running; `audio_passthru_->restoreDefaultPlaybackDevice()` |

**`isTimerRunning()` is used throughout as the canonical "FxSound is processing" predicate**
(`.cpp:1139`, `1147`, `2126`, `698`).

### 10.3 The 100 ms timer — `timerCallback()` — `FxController.cpp:2052-2117`

```
1. if (output_changed_) { output_changed_ = false; Thread::sleep(200); return; }
2. audio_passthru_->processTimer();
3. t = dfx_dsp_.getTotalAudioProcessedTime();              // milliseconds, monotonic
   if (t != audio_process_time_) { audio_process_time_ = t; on_ctr++; off_ctr = 0; }
   else                          {                          off_ctr++; on_ctr  = 0; }
4. if (on_ctr  == 5 && !audio_process_on_)  → ENTER "processing"
   if (off_ctr == 5 &&  audio_process_on_)  → LEAVE "processing"
5. if (++auto_save_counter_ >= 600) { if (preset_dirty_) autoSavePreset(sel); auto_save_counter_ = 0; }
6. now = Time::getCurrentTime();
   if (auto_updates_ && now.hour==10 && now.minute==0 && now.second==0) checkUpdates();
```

ENTER (`.cpp:2076-2087`): `audio_process_on_ = true`; `tray.setStatus(power, true)`;
`window.setIcon(power, true)`; `window.startLogoAnimation()`; and in Pro view
`showProView()` + `startVisualizer()`.

LEAVE (`.cpp:2088-2099`): mirror image, with `stopLogoAnimation()` and `pauseVisualizer()`.

**Debounce = exactly 5 consecutive ticks = 500 ms** in each direction, and the `== 5` equality
means the transition fires exactly once per crossing (the counters keep incrementing past 5 and
are only zeroed by the opposite branch).

`Thread::sleep(200)` in step 1 blocks the **message thread** — the UI is frozen for 200 ms after
every output switch. Do not port this; use a "skip the next N ticks" counter instead.

### 10.4 Audio-processed-time accounting

`DfxDsp::getTotalAudioProcessedTime()` (`DfxDsp.h:71`) returns
`cast_handle->ul_total_msecs_audio_processed_time` (`dsp/ptutil/dfxp/dfxpGet.cpp:362-382`), an
`unsigned long` millisecond counter incremented on the **real-time audio thread**:

```c
r_num_secs = (realtype)i_num_sample_sets / (realtype)last_called_srate;   // dfxpUniversal.cpp:352
ul_msecs_processed_by_buffer = (long)(r_num_secs * 1000);                 // :353
if (!bypass_all)
    cast_handle->ul_total_msecs_audio_processed_time += ul_msecs_processed_by_buffer;  // :358-360
```

Initialised to `0` (`dsp/ptutil/dfxp/dfxpInit.cpp:202`) and settable via
`dfxpSetTotalAudioProcessedTime` (`dsp/ptutil/dfxp/dfxpSet.cpp:459`) — the controller never resets
it. It is read without any synchronisation from the GUI thread: a benign but real data race, and
the counter is expected to wrap.

Note the counter **stops advancing while bypassed**, so "bypassed" and "silent" are
indistinguishable to the controller. That is the whole detection mechanism: *the tray icon turns
blue/red because a millisecond counter moved.*

### 10.5 Spectrum data

`getSpectrumBandValues(Array<float>& out)` — `FxController.cpp:2887-2905`: reads
`NUM_SPECTRUM_BANDS = 10` floats from `dfx_dsp_.getSpectrumBandValues()`. When
`audio_process_on_` is false, it overwrites **every** band with the literal `0.01`. The consumer
(`FxVisualizer::update`, `FxVisualizer.cpp:107-129`) clamps anything outside `[0,1]` to `0` and
feeds a 10-bar-per-band scrolling history (`NUM_BARS = 10`, `FxVisualizer.h:53`), redrawing on
VBlank throttled to `1.0/30.0 s` (JUCE 8) or `setFramesPerSecond(30)` / `10` when paused
(`FxVisualizer.cpp:47-96`).

---

## 11. Hotkeys

### 11.1 Encoding

A hotkey is packed into one `int` settings value:

```
value = (mod << 16) | vk            // FxController.cpp:2213
mod   = (value >> 16) & 0x7         //                 :2179
vk    =  value & 0xff               //                 :2180
```

`mod` uses the Win32 `MOD_*` bits: `MOD_ALT = 0x1`, `MOD_CONTROL = 0x2`, `MOD_SHIFT = 0x4`
(`MOD_WIN = 0x8` is masked off by the `& 0x7`). `vk` is a Win32 virtual-key code.

`getHotkey()` returns `true` only when **`mod == (MOD_CONTROL|MOD_ALT) == 3` or
`mod == (MOD_CONTROL|MOD_SHIFT) == 6`** *and* `vk ∈ [0x30,0x39] ∪ ['A','Z']` (0x41–0x5A);
otherwise it zeroes both outputs and returns `false` (`.cpp:2176-2190`).

### 11.2 Default bindings

| Settings key | Default int | Hex | mod | vk | Binding | Win32 id | Action |
|---|---|---|---|---|---|---|---|
| `cmd_on_off` | `393297` | `0x00060051` | 6 = Ctrl+Shift | `0x51` = `Q` | **Ctrl+Shift+Q** | `1001` | toggle power |
| `cmd_open_close` | `393285` | `0x00060045` | 6 | `0x45` = `E` | **Ctrl+Shift+E** | `1002` | show/hide main window |
| `cmd_next_preset` | `393281` | `0x00060041` | 6 | `0x41` = `A` | **Ctrl+Shift+A** | `1003` | next preset (wraps) |
| `cmd_previous_preset` | `393306` | `0x0006005A` | 6 | `0x5A` = `Z` | **Ctrl+Shift+Z** | `1004` | previous preset (wraps) |
| `cmd_change_output` | `393303` | `0x00060057` | 6 | `0x57` = `W` | **Ctrl+Shift+W** | `1005` | next output device |

(defaults `Settings.cpp:34-38`; ids `FxController.h:219-223`)

### 11.3 Actions — `eventCallback`, `WM_HOTKEY` — `FxController.cpp:1923-2014`

| id | Behaviour |
|---|---|
| `CMD_ON_OFF` | only when `!isRemoteSession()`: toggle power, then `pushMessage(FormatString("FxSound is %s.", on/off))` (`.cpp:1925-1935`) |
| `CMD_OPEN_CLOSE` | `main_window_->isOnDesktop()` ? `hideMainWindow()` : `showMainWindow()` (`.cpp:1936-1946`) |
| `CMD_NEXT_PRESET` | only when power is on and `presetCount > 1`: `sel+1`, wrapping to `0` (`.cpp:1947-1964`) |
| `CMD_PREVIOUS_PRESET` | only when power is on and `presetCount > 1`: `sel-1`, wrapping to `count-1` (`.cpp:1965-1982`) |
| `CMD_NEXT_OUTPUT` | find the index of the selected output in `active_output_devices_`, then advance (wrapping) at most `size()` times until a device with `deviceNumChannel >= 2` is found, and `setOutput(index)` (`.cpp:1983-2013`) |

### 11.4 Validation — `isValidHotkey(mod, vk)` — `FxController.cpp:2271-2300`

1. Reject if `MOD_CONTROL` is absent.
2. Build a synthetic keyboard state (`VK_CONTROL` down, plus `VK_MENU`/`VK_SHIFT` per `mod`) and
   call `ToUnicodeEx(vk, 0, state, out, 3, 0, GetKeyboardLayout(0))`.
3. If it yields a character `>= 0x20`, the combination produces a printable glyph on the current
   layout (classically Ctrl+Alt == AltGr) and is **rejected**.

### 11.5 Registration

* `registerHotkeys()` (`.cpp:2820-2872`): for each of the five commands, if `getHotkey()` and
  `isValidHotkey()`, call `RegisterHotKey(hwnd, id, mod, vk)`. Return value ignored. Sets
  `hotkeys_registered_ = true`.
* `unregisterHotkeys()` (`.cpp:2874-2885`): `UnregisterHotKey` for all five ids.
* `enableHotkeys(bool)` (`.cpp:2162-2174`): writes `settings("hotkeys")`, updates
  `model.setHotkeySupport()`, then registers or unregisters.
* `setHotkey(command, mod, vk)` (`.cpp:2192-2269`):
  1. reject if any **other** command already uses the same `(mod, vk)` — returns `false`
     *without changing anything*;
  2. `code = (mod<<16)|vk`, forced to `0` if `!isValidHotkey`;
  3. `UnregisterHotKey(id)`; if `code == 0` persist `0` and return `false`;
  4. `RegisterHotKey(...)`: on success persist `code` and return `true`; on failure persist `0`
     and return `false`.

UI: `FxHotkeyEditor::keyPressed` (`FxHotkeyLabel.cpp:76-173`) — `Delete` clears the binding;
`Ctrl` is mandatory and **`Alt` wins over `Shift`** when both are held (`if alt … else if shift`,
`:95-102`); only `0-9` and `A-Z` are accepted; it re-checks all five commands *including itself*,
so re-pressing the current binding is a silent no-op. Tooltip:
`"Press Ctrl + Alt/Shift + 0-9/A-Z to change the hotkey"` (`:70`). Display text is
`" Ctrl + [Alt + ][Shift + ]<char>"`, or `"Not configured"` when `mod == 0` (`:224-257`).

Settings-dialog labels (`FxSettingsDialog.cpp:343-344`), in this order:
`"Turn FxSound On/Off"`, `"Open/Close FxSound"`, `"Use Next Preset"`, `"Use Previous Preset"`,
`"Change Playback Device"`. The master toggle is inverted: `"Disable keyboard shortcuts"`
(`:340`, `:384-391`).

---

## 12. Windows session / power events — `eventCallback`

`FxController.cpp:1916-2050`. The message window receives:

| Message | `wParam` | Handler | Cite |
|---|---|---|---|
| `WM_POWERBROADCAST` | `PBT_APMSUSPEND` | `onSystemSuspend()` → if power on, `audio_passthru_->mute(true)` | `.cpp:2019-2022`, `2145-2151` |
| `WM_POWERBROADCAST` | `PBT_APMRESUMESUSPEND` or `PBT_APMRESUMEAUTOMATIC` | `onSystemResume()` → if power on, `mute(false)` | `.cpp:2023-2026`, `2153-2159` |
| `WM_WTSSESSION_CHANGE` | `WTS_SESSION_DESKTOP_READY` (or `WTS_CONSOLE_CONNECT` on Windows 7) **or** `WTS_SESSION_UNLOCK` | `setPowerState(settings("power"))` | `.cpp:2032-2037` |
| `WM_WTSSESSION_CHANGE` | `WTS_CONSOLE_DISCONNECT` | `powerOn(false)` **only** — the model's power flag and the setting are left untouched | `.cpp:2038-2041` |
| `WM_WTSSESSION_CHANGE` | `WTS_REMOTE_CONNECT` | `setPowerState(settings("power"))` — which then hits the remote-session branch and forces power off | `.cpp:2042-2045` |

Everything else falls through to `DefWindowProc` (`.cpp:2049`).

---

## 13. Window / view management

| Function | Behaviour | Cite |
|---|---|---|
| `showView()` | `main_window_->showProView()` or `showLiteView()` per `view_` | `.cpp:878-888` |
| `switchView()` | toggles `Pro ⇄ Lite`, calls the matching `show*View()`, persists `view` | `.cpp:890-904` |
| `hideMainWindow()` | if on desktop: `removeFromDesktop()`, `setVisible(false)`, `run_minimized = true`. Then **once per process** (`minimize_tip_`): after **2000 ms**, push `"FxSound in system tray\r\nClick FxSound icon to reopen"` | `.cpp:911-927` |
| `showMainWindow()` | `run_minimized = false`; `main_window_->show()`; `setIcon(power, audio_process_on_)`; then the survey logic (§14.2) | `.cpp:929-963` |
| `isMainWindowVisible()` | `isOnDesktop() && isVisible()` | `.cpp:965-973` |
| `setMenuClicked(bool)` | persists `menu_clicked` and mirrors into the model (suppresses the one-time menu help bubble) | `.cpp:975-979`, `FxMainWindow.cpp:590-600` |
| `saveWindowPosition(x,y)` / `getWindowPosition(x,y)` | `window_x` / `window_y`, default `0` | `.cpp:2629-2639` |
| `setAlwaysOnTop(bool)` | persists and calls `main_window_->setAlwaysOnTop()` | `.cpp:2782-2787` |
| `setThemeMode(mode)` | no-op if unchanged; else `FxTheme::setThemeMode`, persist `theme_mode`, `setLanguage(getLanguage())` **to force a font reload**, `sendLookAndFeelChange()`, refresh window + tray icons | `.cpp:2760-2775` |
| `getSystemTrayWindowPosition(w,h)` | delegates to the tray view — used to place the custom notification popup near the tray | `.cpp:986-989` |

---

## 14. Network, updates, telemetry, nags

### 14.1 Every outbound URL / process launch in this subsystem

| Trigger | Target | Cite |
|---|---|---|
| version change on startup | notification link `https://www.fxsound.com/changelog` | `.cpp:719` |
| device-error modal | `https://www.fxsound.com/learning-center/installation-troubleshooting` | `.cpp:64` |
| device-error modal | `https://www.fxsound.com/support` | `.cpp:73` |
| survey nag | `https://forms.gle/ATx1ayXDWRaMdiR59` | `.cpp:957` |
| automatic update check | `ChildProcess::start("updater.exe /silent")` | `.cpp:2623-2624` |
| menu "Check for updates" | `ChildProcess::start("updater.exe /checknow")` | `FxMainWindow.cpp:484-487` |
| menu "Download Bonus Presets" | `https://www.fxsound.com/presets` | `FxMainWindow.cpp:479-482` |
| menu / tray "Donate" | `https://www.paypal.com/donate/?hosted_button_id=JVNQGYXCQ2GPG` | `FxMainWindow.cpp:489-492`, `FxSystemTrayView.cpp:280-283` |

**There is no telemetry, analytics, crash upload or phone-home in this subsystem.** The only
network activity is the external `updater.exe` (not in this tree) and user-initiated browser
launches. The minidump (`Main.cpp:154`) stays on disk.

### 14.2 `checkUpdates()` — `FxController.cpp:2612-2627`

```
if (isAudioProcessing()) return;                       // never interrupt playback
now = time(nullptr); last = settings("last_update_time", 0)
if (now - last > 24*60*60) {                           // 86400 s
    settings("last_update_time") = now
    ChildProcess().start("updater.exe /silent")
}
```
Invoked from the timer at **exactly 10:00:00 local time** when `auto_updates_` is on
(`.cpp:2113-2116`) — with a 100 ms tick this is a one-in-ten chance of hitting the exact second,
so the check is unreliable by construction. Fix it in the port (compare against a stored
"next due" instant instead).

### 14.3 Survey nag — `showMainWindow()` — `FxController.cpp:939-961`

```
if (survey_tip_) {
    t = settings("survey_timer")
    if (t == 0) settings("survey_timer") = now + 7*24*60*60      // 604800 s
    else if (now > t) {
        survey_tip_ = false; settings("survey_displayed") = true
        if (authenticated_) pushMessage(
            "Thanks for using FxSound! Would you be\r\ninterested in helping us by taking a quick "
            "4 minute\r\nsurvey so we can make FxSound better?",
            { "Take the survey.", "https://forms.gle/ATx1ayXDWRaMdiR59" })
    }
}
```

### 14.4 Notification catalogue

Every `pushMessage` in the controller, in source order:

| Message (English) | Link | Cite |
|---|---|---|
| `" "` (single space) | `"Click here to see what's new on this version!"` → changelog | `.cpp:719` |
| `"FxSound in system tray\r\nClick FxSound icon to reopen"` | — | `.cpp:924` |
| survey text (§14.3) | `"Take the survey."` | `.cpp:957` |
| `"Preset: " + name` | — | `.cpp:1101` |
| `"Output: " + name` (+ `"\nPreset: " + p`) | — | `.cpp:1120`, `1132`, `1158` |
| `"Output Disconnected"` | — | `.cpp:1170` |
| `"Changes to preset %s are saved."` | — | `.cpp:1221` |
| `"New preset %s is saved."` | — | `.cpp:1234` |
| `"Reached the limit on new presets."` | — | `.cpp:1239` |
| `"Preset %s is deleted."` | — | `.cpp:1313` |
| `"Presets are restored to factory defaults"` | — | `.cpp:1381` |
| `"FxSound is %s."` (`"on"` / `"off"`) | — | `.cpp:1932-1933` |

`FormatString(format, arg)` is `swprintf_s(wchar_t[1024], format, arg)` (`.cpp:2907-2914`) — a
format string taken from the **translation file** and applied to a user-controlled preset name.
A malformed `%s`-less or `%d`-bearing translation is a crash/format-string bug. Replace with a
positional-argument formatter in Rust.

Presentation (`FxSystemTrayView::showNotification`, `FxSystemTrayView.cpp:384-420`): because
`custom_notification_` is hard-coded `true` (`:32`), **every** notification uses the custom
`FxNotification` window unless `SHQueryUserNotificationState() != QUNS_ACCEPTS_NOTIFICATIONS`
(presentation mode / full-screen / quiet hours), in which case it is silently dropped. The Shell
balloon branch (`:407-417`, `NIIF_NOSOUND | NIIF_RESPECT_QUIET_TIME`) is therefore dead.

`FxNotification` geometry (`FxNotification.h:33-47`): `WIDTH = 216`, `HEIGHT = 80`,
`MAX_WIDTH = 560`, `MAX_HEIGHT = 120`, icon `79×12` at `(15,10)`; at most **3** lines, each 20 px
high, final height `line_count * 20 + 60` (`FxNotification.cpp:144`); text x-inset `40` when
autohiding, `20` otherwise (`:152`); fade-in **200 ms** (`:183`); auto-hide after **7000 ms**
without a link, **8000 ms** with one (`:185-192`); corner radius 16, shadow radius 5 (`:204-212`).

---

## 15. Localisation

`setLanguage(code)` (`.cpp:2330-2469`): empty → `"en"`; store in `language_` and
`settings("language")`; clear the current mappings, then a 30-arm `startsWithIgnoreCase`
if/else chain loading a `BinaryData::FxSound_<xx>_txt` blob; finally `theme->loadFont(language_)`
and `main_window_->sendLookAndFeelChange()`.

Order matters: **`"pt-br"` is tested before `"pt"`** (`.cpp:2354`, `:2358`). `"zh-CN"` and
`"zh-TW"` are separate arms (`:2366`, `:2370`). English has no arm — it is the fall-through
(no mappings installed).

The UI cycles through this exact list (`FxLanguage.cpp:25`), 30 entries:

```
en, ar, ba, hr, cs, de, es, fi, fr, hu, id, it, ja, ko, nl, no,
fa, pl, pt, pt-br, ro, ru, sl, sv, th, tr, ua, vi, zh-CN, zh-TW
```

`getLanguageName(code)` maps each to its endonym (`.cpp:2471-2594`), e.g. `pt-br` →
`"português brasileiro"`, `ua` → `"українська"`, `cs` → `"Česky"`. Note `ua` is used where the
ISO code is `uk`, and `ba` is used for Bosnian where the ISO code is `bs` — keep the wrong codes
if you need settings compatibility, or migrate explicitly.

---

## 16. Error and edge-case paths (exhaustive)

| # | Condition | Behaviour | Cite |
|---|---|---|---|
| E1 | `audio_passthru_->init() != 0` | modal `"Error in system audio configuration. Unable to run FxSound"` → quit | `.cpp:704-711` |
| E2 | FxSound virtual device absent at startup (`!dfx_enabled_`) **and** not a remote session | main window removed from desktop, modal `FxDeviceErrorMessage`, quit | `.cpp:729-736` |
| E3 | Virtual device disappears at runtime while processing | `stopTimer()`; same modal + quit (skipped in a remote session) | `.cpp:1630-1641` |
| E4 | Remote Desktop session | power forced off, power button disabled with a tooltip; `--power` ignored; the on/off hotkey does nothing | `.cpp:1009-1020`, `367`, `1927`, `FxMainWindow.cpp:413` |
| E5 | `setOutput(id)` finds no matching device | `mute(true)`, `powerOn(false)`, `"Output Disconnected"` | `.cpp:1165-1171` |
| E6 | `isPlaybackDeviceAvailable()` flips | `OutputError`; the combobox entry is disabled and shows an error state | `.cpp:1617-1622`, `FxView.cpp:129-142` |
| E7 | Device change on another logon session | ignored outright | `.cpp:2121-2122` |
| E8 | Timer running but `onSoundDeviceChange(processing=false)` | **nothing happens** (silent gap) | `.cpp:2126-2135` |
| E9 | `setPreset(name)` with an unknown name | returns `false`, nothing changes — e.g. at startup with a stale `settings("preset")` | `.cpp:1032-1046`, `750-751` |
| E10 | `selectPreset(i)` with `i` out of range | `selected_preset_` unchanged, **but the event is still raised** | `FxModel.cpp:70-81` |
| E11 | `getPreset(i)` out of range | returns a default-constructed `Preset{}` (empty name, `type` uninitialised) | `FxModel.cpp:107-115` |
| E12 | `autoSavePreset` on an empty preset name | early return, `preset_dirty_` **stays true** → retried every 60 s | `.cpp:819-820` |
| E13 | `savePreset` pushes two notifications within 2 s | worked around with `Thread::sleep(2000)` on the message thread | `.cpp:1236-1240` |
| E14 | `renamePreset`/`deletePreset`/`resetPresets` file deletion | `SHFileOperation` without `FOF_ALLOWUNDO` → **permanent**, errors suppressed by `FOF_NOERRORUI`; the double-NUL termination guard is skipped when `path.length()+1 >= MAX_PATH` | `.cpp:1259-1267`, `1289-1299`, `1355-1365` |
| E15 | `exportPresets` target exists | per-file confirmation dialog; declined files are skipped | `.cpp:1401-1407` |
| E16 | `importPresets` name collision | file is skipped and reported in `skipped_presets` | `.cpp:1441-1444` |
| E17 | User preset count reaches `max_user_presets_` | "Save New Preset" menu item disabled; `--save_preset` refused; notification on reaching the cap | `FxMainWindow.cpp:536`, `.cpp:407`, `1236-1240` |
| E18 | `setEffectValue` outside `[0,10]`, `setEqBandBoostCut` outside `[-12,+12]`, `setEqBandFrequency` outside the band's range | silently ignored, no clamping, no error | `.cpp:1758-1761`, `1900-1903`, `1855-1858` |
| E19 | `getEqBandFrequency`/`getEqBandBoostCut` with `band >= numBands` | returns `0` | `.cpp:1837-1847`, `1884-1894` |
| E20 | `getEqBandFrequencyRange` with `band >= numBands` | writes `0` into both out-params | `.cpp:1871-1882` |
| E21 | Unhandled C++/SEH exception | minidump + symbolised stack to the log; `initialise()` also catches `std::exception` and `...` and quits | `Main.cpp:52-92`, `145-306` |
| E22 | `device_configs` schema `!= 2` | list rebuilt from scratch — **all per-device preset assignments are lost** | `.cpp:1467-1472` |

---

## 17. Global state machine

### 17.1 States

| State | `dfx_enabled_` | remote? | `model.power_state_` | `isTimerRunning()` | `audio_process_on_` |
|---|---|---|---|---|---|
| **Fatal** — no virtual device | 0 | 0 | — | — | — |
| **Locked off** — remote session | × | 1 | 0 | 0 | 0 |
| **Off** | 1 | 0 | 0 | 0 | 0 |
| **On, idle** | 1 | 0 | 1 | 1 | 0 |
| **On, processing** | 1 | 0 | 1 | 1 | 1 |
| **Switching output** | 1 | 0 | 1 | 1 | (held) |

### 17.2 Transitions

| From | Trigger | To | Actions |
|---|---|---|---|
| *(start)* | `init()`, virtual device missing | Fatal | modal + quit (`.cpp:729-736`) |
| *(start)* | `init()`, `isRemoteSession()` | Locked off | `enablePowerButton(false)`, `powerOn(false)` (`.cpp:1009-1020`) |
| *(start)* | `init()`, `settings("power") == false` | Off | (`.cpp:746`) |
| *(start)* | `init()`, `settings("power") == true` | On, idle | `dfx_dsp_.powerOn(true)`, `startTimer(100)` (`.cpp:1731-1736`) |
| Off | power button / tray / `CMD_ON_OFF` / `--power 1` | On, idle | as above + persist `power` (`.cpp:1024-1029`) |
| On,* | power off | Off | `dfx_dsp_.powerOn(false)`, `stopTimer()`, `restoreDefaultPlaybackDevice()` (`.cpp:1740-1747`) |
| On, idle | 5 consecutive ticks where the processed-time counter advanced | On, processing | tray+window icon "processing", `startLogoAnimation()`, Pro: `showProView()` + `startVisualizer()` (`.cpp:2076-2087`) |
| On, processing | 5 consecutive ticks with no advance | On, idle | mirror image + `pauseVisualizer()` (`.cpp:2088-2099`) |
| On,* | `setOutput` reaches `setAsPlaybackDevice` | Switching output | `output_changed_ = true` (`.cpp:1155-1156`) |
| Switching output | next tick | previous state | consume the flag, `Thread::sleep(200)`, skip the tick (`.cpp:2054-2059`) |
| On,* | output not found | Off | `mute(true)`, `powerOn(false)`, "Output Disconnected" (`.cpp:1165-1171`) |
| On,* | `PBT_APMSUSPEND` | On,* (muted) | `mute(true)` (`.cpp:2145-2151`) |
| On,* (muted) | `PBT_APMRESUME*` | On,* | `mute(false)` (`.cpp:2153-2159`) |
| any | `WTS_CONSOLE_DISCONNECT` | Off (model flag untouched) | `powerOn(false)` (`.cpp:2038-2041`) |
| any | `WTS_SESSION_DESKTOP_READY` / `WTS_SESSION_UNLOCK` / `WTS_REMOTE_CONNECT` | per `settings("power")` | `setPowerState(...)` (`.cpp:2032-2045`) |
| On, processing | virtual device vanishes | Fatal | `stopTimer()` + modal + quit (`.cpp:1630-1641`) |

### 17.3 Narrative

FxSound boots, reads its settings file, and asks the passthru layer for the device list. If the
FxSound virtual audio device is not among them, the app is useless and it says so and dies. If
this is a Remote Desktop session, the whole DSP path is unavailable, so power is nailed to off
and the power button is disabled with an explanatory tooltip; the app still runs as a settings
editor.

Otherwise it picks an output device, applies whatever preset that device is configured for (or
the last-used one), and — if the persisted power flag says so — turns the DSP on. "On" means two
things: `DfxDsp::powerOn(true)`, and a 100 ms JUCE timer.

That timer is the app's heartbeat. Each tick it pumps the passthru layer, then samples a
millisecond counter that the real-time audio thread increments every time it processes a
non-bypassed buffer. Five consecutive ticks of movement (500 ms) means "audio is flowing":
the tray icon goes blue (light theme) or red (dark theme), the logo animates, and the spectrum
visualizer starts. Five consecutive ticks of stillness means silence: everything goes back to the
white "on but idle" icon. Every 600 ticks (60 s) the timer flushes any unsaved preset edit to an
autosave file, and at 10:00:00 local time it may shell out to `updater.exe /silent`.

Preset editing never writes the preset file. Moving an effect or EQ slider sets two flags: a
per-preset `modified` bit (which paints a `*` next to the name everywhere) and a controller-level
`preset_dirty_` bit (which schedules an autosave). The autosave is a shadow `.fac` in
`AutoSave\<name>.fac`. Because `initPresets` derives `modified` purely from the existence of that
shadow file, and `setPreset` prefers the shadow over the original, unsaved edits survive a
restart, a crash, and switching back and forth between presets. "Undo" is simply: delete the
shadow, clear the bit, reload the original.

Device hot-plug arrives asynchronously on a COM notification thread. If it belongs to another
logon session it is dropped. Otherwise, when FxSound is on, the controller recomputes the active
device list, and — only if the device *count* changed — decides whether a newly-arrived device
outranks the current one in the user's priority list; if it does, it switches, which also switches
to that device's configured preset. When FxSound is off, the controller instead just shadows the
system default device so that the UI never lies about where audio is going.

Everything the UI sees goes through `FxModel`, which owns nine event types and posts every
notification asynchronously to the message thread. Notifications to the *user* go through a
single-slot mailbox on the same model, which is why the code occasionally sleeps for two seconds
to avoid losing a message.

---

## 18. Rust / egui / PipeWire re-design

### 18.1 Thread topology

```
 ┌──────────────────────────────────────────────────────────────────────────┐
 │ T1  GUI thread — winit event loop, eframe::App::update()                  │
 │     owns AppModel (all of FxModel + all of FxController's GUI state)      │
 │     100 ms tick via ctx.request_repaint_after                             │
 └───┬───────────────┬──────────────────┬───────────────────┬───────────────┘
     │ params_tx     │ io_tx            │ ctl_tx            │ ▲ ui_rx (all events in)
     │ (triple_buffer│ (crossbeam)      │ (crossbeam)       │ │ crossbeam::Receiver<UiEvent>
     │  Input)       │                  │                   │ │
     ▼               ▼                  ▼                   │ │
 ┌───────────┐  ┌──────────┐  ┌─────────────────────┐       │ │
 │ T2  RT    │  │ T3  I/O  │  │ T4  PipeWire main   │       │ │
 │ PipeWire  │  │ blocking │  │     loop (registry, │───────┼─┘
 │ process() │  │ preset & │  │     metadata,       │       │
 │ callback  │  │ settings │  │     node add/remove)│       │
 │           │  │ file IO  │  └─────────────────────┘       │
 │ atomics ──┼──┴──────────┴────────────────────────────────┘
 │ + spectrum triple_buffer::Output
 └───────────┘
 ┌─────────────────────────────────────────────────────────────────────────┐
 │ T5  D-Bus (zbus) — logind sleep/lock signals, StatusNotifierItem (tray), │
 │     org.freedesktop.Application activation (single instance),            │
 │     XDG GlobalShortcuts portal                                           │
 └─────────────────────────────────────────────────────────────────────────┘
```

### 18.2 Crate choices (opinionated)

| Concern | Crate | Why |
|---|---|---|
| UI | `eframe` 0.36 (`wgpu` backend, `wayland` feature) | as mandated; `glow` as the fallback for old Mesa |
| GUI↔RT bulk params | `triple_buffer` 8 | wait-free, allocation-free on both ends; perfect for a ~2 KB `DspParams` POD |
| RT→GUI scalars | `std::sync::atomic::AtomicU64` / `AtomicU32` + `bytemuck`-free manual f32 bit casts, or `atomic_float::AtomicF32` | processed-ms counter, xrun counter |
| RT→GUI spectrum | `triple_buffer::TripleBuffer<[f32; 10]>` | matches `NUM_SPECTRUM_BANDS = 10` |
| Cross-thread events | `crossbeam-channel` (bounded, `try_send` on the producer) | never block, never allocate on the RT side |
| Snapshot config for RT | `arc-swap::ArcSwap<Arc<PresetSnapshot>>` | only for the *rare* whole-preset swap; the RT side does `load()` (a `Guard`, no refcount bump on the fast path) |
| Audio | `pipewire` 0.8 (`pipewire-rs`) | native PipeWire node + registry |
| Settings | `serde` + `toml_edit` + `directories` + `tempfile` (atomic rename) | replaces `ApplicationProperties` |
| Tray | `ksni` | StatusNotifierItem over D-Bus; the only thing that works on Wayland |
| Desktop notifications | `notify-rust` | replaces both the Shell balloon and the custom popup |
| D-Bus | `zbus` 5 | logind, SNI, app activation, portals |
| Portals (global shortcuts, file chooser) | `ashpd` | `org.freedesktop.portal.GlobalShortcuts`, `FileChooser` for import/export |
| CLI | `clap` (derive) | replaces `juce::ArgumentList`; keep the exact long-option spellings from §6 |
| i18n | `fluent` + `fluent-bundle`, or `rust-i18n` for a 1:1 port of the flat key/value `.txt` files | the JUCE files are `"src" = "dst"` pairs — trivially convertible |
| Logging | `tracing` + `tracing-appender` (rolling) | replaces `FileLogger` |
| Crash capture | `minidumper` + `crash-handler` (optional) | the Windows build writes a minidump; decide whether Linux needs one |

### 18.3 Ownership split

```rust
// ── T1: GUI thread. Single-threaded, `!Send` is fine. ────────────────────────
pub struct App {
    // == what was FxModel ==
    power: bool,                      // FxModel.h:192
    presets: Vec<Preset>,             // FxModel.h:193 — app presets first, then user presets
    selected_preset: usize,           // FxModel.h:195
    outputs: Vec<SinkInfo>,           // FxModel.h:202 — active, >=2ch, priority-sorted
    selected_output: Option<SinkId>,  // FxModel.h:203
    menu_clicked: bool,               // FxModel.h:199
    hotkey_support: bool,             // FxModel.h:198

    // == what was FxController (GUI-side) ==
    view: ViewType,                   // Lite=1 | Pro=2
    language: String,                 // "en" | "pt-br" | "zh-CN" | ...
    theme: ThemeMode,                 // Dark=0 | Light=1
    always_on_top: bool,
    hide_help_tooltips: bool,
    hide_notifications: bool,
    auto_updates: bool,
    max_user_presets: u32,            // clamp [10,120], default 120
    preset_dirty: bool,               // FxController.h:287
    auto_save_counter: u32,           // ticks toward 600
    audio_process_time_ms: u64,       // last sampled RT counter
    on_counter: u8, off_counter: u8,  // the 5-tick debounce
    audio_process_on: bool,
    minimize_tip: bool,               // one-shot
    survey_tip: bool,
    settings: Settings,               // serde-backed, dirty-flag + debounced flush

    // == boundaries ==
    params_tx: triple_buffer::Input<DspParams>,
    spectrum_rx: triple_buffer::Output<[f32; 10]>,
    processed_ms: Arc<AtomicU64>,     // written by T2
    io_tx: crossbeam_channel::Sender<IoJob>,
    pw_tx: crossbeam_channel::Sender<PwCommand>,
    ui_rx: crossbeam_channel::Receiver<UiEvent>,
    egui_ctx: egui::Context,          // cloned into T3/T4/T5 for request_repaint()
}

// ── Shared, POD, no heap, no Drop. Published GUI → RT. ───────────────────────
#[derive(Clone, Copy)]
#[repr(C)]
pub struct DspParams {
    pub power:           bool,
    pub effects:         [f32; 5],    // Fidelity, Ambience, Surround, DynamicBoost, Bass — 0.0..=10.0
    pub num_eq_bands:    u8,          // one of 5,10,15,20,31
    pub eq_freq_hz:      [f32; 31],   // only the first num_eq_bands are meaningful
    pub eq_gain_db:      [f32; 31],   // -12.0 ..= +12.0
    pub master_gain_db:  f32,         // -20.0 ..= +20.0, 1.0 steps
    pub volume_level_db: f32,         //   0.0 ..=   4.0, 0.5 steps
    pub filter_q:        f32,         //   1.0 ..=   3.0, 0.5 steps
    pub balance_db:      f32,         // -20.0 ..= +20.0, 1.0 steps
    pub generation:      u64,         // bump on every publish; RT uses it to decide "re-ramp"
}

// ── T2: RT thread. Nothing here allocates, locks, logs, or touches the fs. ───
struct RtState {
    params_rx:   triple_buffer::Output<DspParams>,
    spectrum_tx: triple_buffer::Input<[f32; 10]>,
    processed_ms: Arc<AtomicU64>,
    smoothed:    SmoothedParams,      // one LinearRamp per continuous parameter
    dsp:         DspGraph,            // biquad cascade + effect chain, pre-sized for 31 bands
}
```

### 18.4 The RT boundary, concretely

**Rule 1 — the RT thread never reads a file, never allocates, never takes a lock.**
Preset loading happens on T3 (parse the `.fac`), is turned into a `DspParams` on T1, and is
published through the triple buffer. That replaces `dfx_dsp_.loadPreset(path)` being called
directly from the GUI thread (`FxController.cpp:1073`, `1078`) — which in the C++ build is only
safe because the DSP takes an internal lock the audio thread also takes.

**Rule 2 — every continuous parameter is ramped on the RT side.** Mirror the C++ quantisation
(§8.4) at the *UI* layer so the persisted values match byte-for-byte, then ramp between
quantised targets over ~10 ms:

```rust
impl RtState {
    fn process(&mut self, frames: usize, sample_rate: u32, buf: &mut [f32]) {
        if let Some(p) = self.params_rx.read_if_updated() {   // wait-free, no alloc
            self.smoothed.retarget(p, sample_rate);           // sets ramp targets only
            if p.generation != self.last_generation {         // band count / preset change
                self.dsp.reconfigure_in_place(p);             // pre-allocated storage only
                self.last_generation = p.generation;
            }
        }
        self.dsp.run(buf, frames, &mut self.smoothed);
        if !self.smoothed.bypassed() {
            // mirrors dfxpUniversal.cpp:352-360 exactly
            let ms = (frames as u64 * 1000) / sample_rate as u64;
            self.processed_ms.fetch_add(ms, Ordering::Relaxed);
        }
        self.spectrum_tx.write(self.dsp.spectrum_10_bands());
    }
}
```

**Rule 3 — the GUI polls, it is never called back from RT.** The 100 ms tick on T1 does:

```rust
fn tick_100ms(&mut self) {
    // 1. (replaces audio_passthru_->processTimer(): nothing to pump on PipeWire)
    // 2. the 5-tick debounce, byte-identical to FxController.cpp:2062-2099
    let t = self.processed_ms.load(Ordering::Relaxed);
    if t != self.audio_process_time_ms {
        self.audio_process_time_ms = t; self.on_counter += 1; self.off_counter = 0;
    } else {
        self.off_counter += 1; self.on_counter = 0;
    }
    if self.on_counter == 5 && !self.audio_process_on  { self.enter_processing(); }
    if self.off_counter == 5 &&  self.audio_process_on { self.leave_processing(); }

    // 3. 600 ticks == 60 s autosave
    self.auto_save_counter += 1;
    if self.auto_save_counter >= 600 {
        if self.preset_dirty { self.io_tx.send(IoJob::AutoSave { idx: self.selected_preset }).ok(); }
        self.auto_save_counter = 0;
    }
    // 4. update check — but see §14.2: use a stored `next_update_due` instant, not hour==10 && min==0 && sec==0
}
```

Drive it with `ctx.request_repaint_after(Duration::from_millis(100))` at the end of every
`update()`, plus a `std::time::Instant` guard so a repaint triggered by input does not double-tick.

**Rule 4 — replace `output_changed_` + `Thread::sleep(200)`** (`FxController.cpp:2054-2059`)
with `skip_ticks: u8`, decremented at the top of `tick_100ms`. Never block the GUI thread.

### 18.5 Event model in Rust

Drop `FxModel::Listener` entirely. egui is immediate-mode: the widgets read `App` fields directly
each frame, so `PresetSelected`, `PresetListUpdated`, `PresetModified`, `OutputSelected`,
`OutputListUpdated` all become *nothing* — you simply render from the current state. Only two
of the nine events survive as real messages:

| C++ event | Rust equivalent |
|---|---|
| `Notification` (1) | `UiEvent::Notify { text: String, link: Option<(String, String)> }` — but push it to `notify-rust` immediately and keep a `VecDeque<Toast>` for the in-window toast, so messages **queue** instead of overwriting (fixes the `Thread::sleep(2000)` hack) |
| `OutputError` (8) | a field: `output_error: Option<OutputError>`; render an inline warning next to the device picker |
| `Subscription` (2) | delete |
| `Other` (9) | delete |

Everything arriving from another thread goes through **one** channel:

```rust
enum UiEvent {
    SinksChanged(Vec<SinkInfo>),         // T4: PipeWire registry add/remove/param change
    DefaultSinkChanged(SinkId),          // T4: `default.audio.sink` metadata changed
    SinkUnavailable(SinkId),             // T4: our target node vanished  → was OutputError
    PresetsRescanned(Vec<Preset>),       // T3: after save/rename/delete/import
    PresetSaved { name: String },        // T3
    IoError(IoErrorKind, String),        // T3
    PrepareForSleep(bool),               // T5: logind
    SessionLocked(bool),                 // T5: logind
    Shortcut(HotkeyCommand),             // T5: GlobalShortcuts portal
    CliCommand(Box<CliArgs>),            // T5: second instance, replaces anotherInstanceStarted
    Notify { text: String, link: Option<(String, String)> },
}
```

T1 drains it non-blockingly at the top of `update()`:
`while let Ok(ev) = self.ui_rx.try_recv() { self.handle(ev); }`. Producers call
`egui_ctx.request_repaint()` after `send` so the UI wakes immediately.

### 18.6 Mapping every Windows mechanism

| Windows mechanism | Cite | Linux / Wayland / PipeWire replacement |
|---|---|---|
| Virtual audio driver + `setAsPlaybackDevice` | `.cpp:1155`, `AudioPassthru.h:73` | Register a PipeWire node with `media.class = "Audio/Sink"`, `node.name = "fxsound"`, `node.description = "FxSound"`. Apps play into it; the process callback runs the DSP; the node's output ports are linked to the chosen physical sink. Then make it the system default by writing `default.audio.sink = "fxsound"` on the `default` metadata object (what `wpctl set-default` does). |
| `restoreDefaultPlaybackDevice()` | `.cpp:1747` | Record the previous `default.audio.sink` at startup and restore that exact value on power-off and on exit. Do this in a `Drop` impl **and** on SIGTERM/SIGINT (`signal-hook`), or a crash leaves the user's audio routed into a dead node. |
| `getSoundDevices()` / `SoundDevice` | `AudioPassthru.h:32-53` | Walk the PipeWire registry for `PW_TYPE_INTERFACE_Node` with `media.class == "Audio/Sink"`. Map: `pwszID` → `node.name` (stable across reconnects; `object.serial` is not); `deviceFriendlyName` → `node.description`; `deviceFormFactor` → `device.form-factor` (falls back to `api.alsa.card` / `device.bus`); `deviceNumChannel` → `audio.channels` from the node's `EnumFormat`; `isDefaultDevice` → equals the metadata default; `isActive` → node state `Running`/`Idle` (not `Suspended` because of an unavailable device profile); `isRealDevice` → `node.name != "fxsound"`. |
| Hot-plug via `IMMNotificationClient` → `onSoundDeviceChange` | `.cpp:2119` | PipeWire registry `global_added` / `global_removed` + `Metadata` property listener, on T4. **These are real events** — throw away the `sound_devices.size() != device_count_` count-delta heuristic (`.cpp:1559`, `1625`, `1681`) and the `checkDeviceChanges()` poll (`.cpp:1199`) entirely. |
| `WTSGetActiveConsoleSessionId()` session filter | `.cpp:2121` | Unnecessary — a PipeWire client only ever sees its own session's graph. Delete. |
| `isPlaybackDeviceAvailable()` | `.cpp:1617` | The target sink's global id disappeared from the registry, or linking to it failed. Raise `UiEvent::SinkUnavailable`. |
| `AudioPassthru::mute(bool)` on suspend/resume | `.cpp:2149`, `2157` | Set the FxSound node's `Props::mute` via `pw_node_set_param(SPA_PARAM_Props)`, or just gate `DspParams.power` — but keep the node alive so clients do not get disconnected. |
| `WM_POWERBROADCAST` / `PBT_APMSUSPEND` / `PBT_APMRESUME*` | `.cpp:2017-2028` | `zbus`: `org.freedesktop.login1.Manager` → `PrepareForSleep(b)` signal. `true` = about to suspend, `false` = resumed. Take an inhibitor lock (`Inhibit("sleep", …, "delay")`) if you need to finish work before the machine sleeps. |
| `WM_WTSSESSION_CHANGE` lock/unlock | `.cpp:2030-2046` | `org.freedesktop.login1.Session` `Lock` / `Unlock` signals, or the `LockedHint` property; `org.freedesktop.ScreenSaver` `ActiveChanged` as a fallback. |
| `SM_REMOTESESSION` | `SysInfo.cpp:138-141` | **No good equivalent, and no need for one.** The Windows lockout exists because WASAPI loopback/exclusive mode is unavailable over RDP; PipeWire over waypipe/xrdp/VNC has no such restriction. **Recommendation: delete the remote-session concept entirely** (and with it E4, the `--power` guard at `.cpp:367`, the `CMD_ON_OFF` guard at `.cpp:1927`, and the disabled-power-button tooltip). If you must keep a hint, read `$XDG_SESSION_TYPE` / `$SSH_CONNECTION`. |
| `RegisterHotKey` / `WM_HOTKEY` | `.cpp:2256`, `1923` | **A Wayland client cannot grab global keys.** Preferred: `org.freedesktop.portal.GlobalShortcuts` via `ashpd` — the app declares shortcut ids (`toggle-power`, `open-close`, `next-preset`, `prev-preset`, `next-output`), the compositor owns the actual binding, and the user rebinds in system settings. Plasma ≥ 5.27 and GNOME 48 support it; on older stacks it is simply absent — detect and disable the hotkey UI (this is exactly what `hotkey_support_` already models, `FxModel.h:198`). Fallbacks, in order of preference: (a) MPRIS2 via `mpris-server` so media keys map to next/previous preset — semantically dubious, but it works everywhere; (b) document a compositor keybinding that runs `fxsound --preset "X"` / `fxsound --power 0`, which the CLI already supports. **Do not** use `XGrabKey` — it only sees X11/XWayland keys and will silently miss everything a Wayland-native client types. |
| `isValidHotkey()` via `ToUnicodeEx` | `.cpp:2271-2300` | Irrelevant: the compositor owns conflict detection under the portal. Delete the function; keep the *settings schema* (`(mod<<16)|vk`) only if you need to migrate existing Windows settings, and translate `vk` 0x30–0x39/0x41–0x5A to `xkb` keysyms. |
| `HKCU\…\CurrentVersion\Run` autostart | `.cpp:2789-2818` | Write `$XDG_CONFIG_HOME/autostart/fxsound.desktop` (`Type=Application`, `Exec=/usr/bin/fxsound --run_minimized`, `X-GNOME-Autostart-enabled=true`). Alternative for systemd-managed sessions: a user unit `fxsound.service` with `WantedBy=graphical-session.target`, toggled with `systemctl --user enable/disable`. `isLaunchOnStartup()` = the file exists and is not `Hidden=true`. |
| Tray icon (`Shell_NotifyIcon`, `NIF_GUID`) | `FxSystemTrayView.cpp:172-214` | `ksni` (StatusNotifierItem). Four icon states map 1:1: gray = power off, white = on/idle, blue = processing (light theme), red = processing (dark theme) (`FxSystemTrayView.cpp:90-111`). Ship them as themed SVGs in the icon theme so the host can pick the right size. |
| Balloon / custom notification window | `FxSystemTrayView.cpp:384-420`, `FxNotification.cpp` | `notify-rust` → `org.freedesktop.Notifications`. A `link` becomes an action button (`add_action("open", "Take the survey.")`). The custom floating popup **cannot be reproduced**: a Wayland client may not position its own surface in global coordinates, so `getSystemTrayWindowPosition()` (`.cpp:986`) has no equivalent. Use the system notification, plus an in-window `egui` toast stack for when the main window is focused. |
| `SHQueryUserNotificationState` (quiet hours) | `FxSystemTrayView.cpp:394-398` | The notification daemon handles Do Not Disturb itself. Just send; stop second-guessing. |
| `setAlwaysOnTop` | `.cpp:2786` | No xdg-shell protocol for a toplevel to request always-on-top. On wlroots compositors you can use `wlr-layer-shell` (`smithay-client-toolkit`) at `Layer::Top`, but that reshapes the whole window model. **Recommendation: hide the option on Wayland** and point the user at their compositor's window rules; keep it functional on X11/XWayland if you support that path. |
| `saveWindowPosition` / `getWindowPosition` | `.cpp:2629-2639` | **Impossible on Wayland** — a client can neither read nor set its own global position. Drop `window_x`/`window_y`; persist the *size* only (`eframe` `ViewportBuilder::with_inner_size`) and let the compositor place the window. |
| `moreThanOneInstanceAllowed=false` + `anotherInstanceStarted` | `Main.cpp:46`, `136` | Own the D-Bus name `org.fxsound.FxSound` with `zbus` (`RequestName` + `DoNotQueue`). Implement `org.freedesktop.Application` (`Activate`, `ActivateAction`, `Open`) plus a private `ApplyConfig(as argv)` method. A second process that fails to take the name calls `ApplyConfig` with its own argv and exits 0. A Unix socket in `$XDG_RUNTIME_DIR/fxsound.sock` is a simpler fallback if you would rather not depend on D-Bus for this. |
| `AttachConsole(ATTACH_PARENT_PROCESS)` for `--status` | `.cpp:684-693` | Unnecessary — the second process already has stdout. Have `ApplyConfig` return the JSON as a D-Bus reply and let the forwarding process print it. Still write `status.json` for scripts. |
| `ApplicationProperties` / `.settings` XML | `Settings.cpp:26-66` | `$XDG_CONFIG_HOME/fxsound/settings.toml` (default `~/.config/fxsound/settings.toml`) via `directories::ProjectDirs`. **Keep the exact key names from §7.3** so a Windows→Linux migration is a mechanical XML→TOML transform. Machine-wide defaults (the `getCommonSettings` layer) → `/etc/fxsound/defaults.toml`, merged under the user file. Write atomically: `tempfile::NamedTempFile` in the same dir + `persist()`. Debounce writes (the C++ code writes a key on every slider tick). |
| `.secure` property file | `Settings.cpp:44-46` | Dead — do not port. |
| `RegDeleteTree(HKCU\Software\DFX)` | `.cpp:717` | Nothing to clean on a fresh Linux port. Keep the *hook* (a `version` key + an on-upgrade migration function), drop the body. |
| `FileLogger` → `%APPDATA%\FxSound\fxsound.log` | `.cpp:154` | `tracing_appender::rolling::daily($XDG_STATE_HOME/fxsound/)`. Also log the `env!("CARGO_PKG_VERSION")`, `uname -srm`, and the PipeWire server version — the Windows build logs version/OS/arch at `.cpp:155-171`. |
| `MiniDumpWriteDump` | `Main.cpp:164` | Optional: `crash-handler` + `minidumper`, or just install a `panic::set_hook` that writes the backtrace into the log. Do not upload anything. |
| `SHFileOperation(FO_DELETE)` | `.cpp:1265`, `1297`, `1363` | `std::fs::remove_file`. If you want the Recycle-Bin semantics the Windows code *does not* have, use the `trash` crate — but the current behaviour is a permanent delete, so match it and just add a confirmation dialog. |
| `%USERPROFILE%\Documents\FxSound\Presets\Export` | `.cpp:1386` | XDG user dir `DOCUMENTS` via `directories::UserDirs::document_dir()`, falling back to `$HOME`; better still, use the `ashpd` `FileChooser` portal so the export location is user-chosen and sandbox-safe. |
| `ChildProcess("updater.exe /silent")` | `.cpp:2624` | There is no self-updater on Linux — packages are. **Recommendation: delete auto-update entirely**, hide the `automatic_updates` toggle, and replace "Check for updates" with a link to the project releases page. If you insist on a check, do an HTTPS GET of a version manifest with `ureq`/`reqwest` on T3 (never on T1) and only *tell* the user. |

### 18.7 Path mapping

| Windows | Linux |
|---|---|
| `%APPDATA%\FxSound\FxSound.settings` | `$XDG_CONFIG_HOME/fxsound/settings.toml` |
| `%ProgramData%\FxSound\FxSound.settings` | `/etc/fxsound/defaults.toml` |
| `<exe dir>\Factsoft\*.fac` | `$XDG_DATA_DIRS/fxsound/presets/*.fac` (i.e. `/usr/share/fxsound/presets`), with a dev fallback next to the binary |
| `%APPDATA%\FxSound\Presets\*.fac` | `$XDG_DATA_HOME/fxsound/presets/*.fac` |
| `%APPDATA%\FxSound\AutoSave\<name>.fac` | `$XDG_STATE_HOME/fxsound/autosave/<name>.fac` — state, not data |
| `%APPDATA%\FxSound\status.json` | `$XDG_RUNTIME_DIR/fxsound/status.json` (it is ephemeral), with `$XDG_STATE_HOME` as the fallback |
| `%APPDATA%\FxSound\fxsound.log` | `$XDG_STATE_HOME/fxsound/fxsound.log` |
| `%USERPROFILE%\Documents\FxSound\Presets\Export` | `$XDG_DOCUMENTS_DIR/FxSound/Presets/Export` |

Preset filenames are derived from the *embedded* preset name (`.cpp:811`, `1435`), which on
Windows was already sanitised against `<>:"/\|?*`. On Linux only `/` and NUL are illegal, but
**keep the Windows sanitiser** so preset files stay portable between the two builds.

### 18.8 What to fix rather than port

1. `Thread::sleep(200)` in `timerCallback` (`.cpp:2057`) → a skip-tick counter.
2. `Thread::sleep(2000)` in `savePreset` (`.cpp:1238`) → a toast queue.
3. The `hour==10 && minute==0 && second==0` update trigger (`.cpp:2113`) → a stored deadline.
4. `swprintf_s` with a translated format string (`.cpp:2911`) → named/positional args (`fluent`).
5. `savePreset("")` writing into the user directory regardless of the preset's type
   (`.cpp:1215-1216`) → assert `type == UserPreset` inside the function.
6. Unsynchronised reads of `audio_process_time_` from the GUI thread → `AtomicU64`.
7. `std::sort` (non-stable) used for the two-key device sort in `initDeviceConfigs`
   (`DeviceConfig.cpp:30-40`) → one `sort_by_key` on a tuple, or `sort_by` with a total order.
8. `updateDeviceConfigs` firing `onDeviceConfigsUpdate` once per missing device
   (`DeviceConfig.cpp:97-107`) → fire once.
9. The `notify` mailbox losing messages (`FxModel.h:170-183`) → a queue.
10. `FxModel` fields `menu_clicked_` and `output_disconnected_` being read before initialisation
    (`FxModel.cpp:21-31` initialises neither) → Rust's type system prevents this for free.
11. `RegSetValueEx(..., sizeof(szPath))` writing the whole `MAX_PATH` buffer (`.cpp:2810`) →
    n/a once you use a `.desktop` file.

### 18.9 Things to port verbatim (behavioural fingerprints)

Keep these bit-exact or users will notice:

* 100 ms tick; 5-tick (500 ms) processing on/off debounce; 600-tick (60 s) autosave.
* Autosave-shadow semantics: `modified` is derived *only* from the existence of
  `autosave/<name>.fac`; `setPreset` prefers the shadow.
* The `" *"` suffix (space, asterisk) on modified preset names in every list
  (`FxView.cpp:164`, `183`, `186`; `FxSystemTrayView.cpp:231`).
* The separator between app presets and user presets in every preset list
  (`FxView.cpp:158-162`, `FxSystemTrayView.cpp:238-242`).
* Preset name: 64 characters max, `<>:"/\|?*` stripped, case-insensitive uniqueness.
* The five quantisation rules in §8.4 and all the range clamps in §6.1.
* Wrap-around order for next/previous preset and next output, including the "skip devices with
  `< 2` channels" loop in `CMD_NEXT_OUTPUT` (`.cpp:1994-2012`).
* Device priority: array order is priority order; an unlisted device sorts last; a *newly
  connected* device only steals the output if it outranks the current one.
* A device's configured preset is applied with `notify = false` (no "Preset:" toast) when the
  switch is automatic, but the name is appended to the "Output:" toast instead
  (`.cpp:1126-1134`).
* Notification auto-hide: 7 s plain, 8 s with a link; 3 lines max.

---

## Open questions / risks for the Rust port

1. **The `.fac` preset format is undocumented here.** `DfxDsp::loadPreset` /
   `savePreset` / `exportPreset` / `getPresetInfo` (`DfxDsp.h:45-47`, `:70`) are opaque to the
   controller — all it ever sees is the embedded `name`. Reverse-engineering the container is a
   separate work item (`dsp/DfxDspPreset.cpp`). Until then the Rust port cannot read existing user
   presets, which is the single biggest migration risk. Decide early: parse `.fac`, or ship a
   converter, or accept a clean break with a new format.
2. **Effect value scaling is inconsistent and undocumented.** `getEffectValue` returns 0..1 while
   `setEffectValue` takes 0..10 (`.cpp:1758` guard vs. `printStatus`'s `*10.0f` at `.cpp:667-671`,
   and `setPreset`'s `value * 10` round-trip at `.cpp:1088`). Confirm the exact DSP-side mapping
   before you define `DspParams::effects`.
3. **What does `DfxDsp::setFilterQ` actually multiply?** The controller only ever passes 1.0–3.0
   in 0.5 steps and calls it a "q_multiplier" (`DfxDsp.h:59`). The per-band base Q is inside the
   DSP. Needed for filter-coefficient parity.
4. **Per-band frequency ranges come from the DSP** (`getEqBandFrequencyRange`, `.cpp:1871-1882`)
   and are not enumerated anywhere in this subsystem. The band-count options are
   `{5,10,15,20,31}`; you must extract the actual centre frequencies and per-band min/max from
   `dsp/DfxDspEq.cpp` before the EQ UI can be built.
5. **PipeWire sink vs. filter-chain.** A `media.class = Audio/Sink` node that also links
   downstream is not the most idiomatic PipeWire shape; a `filter-chain` module or a
   `Audio/Sink` + `Stream/Output/Audio` pair may behave better with WirePlumber's session
   policy (especially around suspend-on-idle and device profile switching). Prototype both and
   measure latency and reconnection behaviour before committing.
6. **Who owns the default sink?** Hijacking `default.audio.sink` fights with WirePlumber's own
   default-device policy and with any user who changes the default from `pavucontrol`. You may
   need to mark the FxSound node with `node.dont-reconnect` / adjust
   `priority.session` / add a WirePlumber rule. There is a real risk of a fight-loop where
   WirePlumber and FxSound each keep resetting the default.
7. **Sample format and channel count.** The Windows DSP processes `short int` (16-bit)
   (`DfxDsp.h:44`). PipeWire will hand you `f32` planar or interleaved at the graph rate.
   Confirm the DSP algorithms are format-agnostic, or you will be porting fixed-point assumptions.
8. **Global shortcuts availability is a moving target.** The `GlobalShortcuts` portal is absent on
   many current setups. Decide the product answer now: degrade silently (like `hotkey_support_`
   already allows), or make the CLI + compositor-binding path a first-class, documented feature.
9. **Always-on-top and window position are simply unavailable** on Wayland (§18.6). Both are
   currently user-visible settings (`always_on_top` in two menus, `window_x`/`window_y`). Removing
   settings is a UX decision, not just an engineering one.
10. **Auto-update has no Linux story.** `updater.exe` is not in this tree at all. Shipping an
    in-app updater on Linux is usually wrong; but `automatic_updates` defaults to `true`
    (`.cpp:193`) and the timer calls it daily, so the behaviour is load-bearing for the existing
    UI. Decide whether the toggle survives.
11. **Remote-session lockout**: recommended for deletion (§18.6), but it currently gates four
    distinct code paths. Confirm with the product owner that Linux users *should* be able to run
    FxSound over VNC/RDP/waypipe.
12. **`device_configs` keys devices by friendly name, not id** (`DeviceConfig.cpp:68`,
    `FxController.cpp:1522`, `2685`, `2708-2711`) even though `device_id` is stored. Two identical
    USB headsets produce one config entry. On PipeWire, `node.name` is a much better key
    (`alsa_output.usb-0b0e_Jabra…`). Changing the key breaks settings compatibility — decide
    whether to migrate or to keep the (buggy) name-keying.
13. **`device_configs_version` bumping wipes all per-device preset assignments**
    (`.cpp:1467-1472`). Write a real migration for version 3 instead of re-seeding.
14. **`getTotalAudioProcessedTime` is an `unsigned long` that wraps** and is deliberately allowed
    to (`dfxpUniversal.cpp:367-370`). An `AtomicU64` will not wrap in any realistic lifetime, so
    the `!=` comparison stays correct — just do not port a wrap-aware comparison you do not need.
15. **The DSP's own thread-safety contract is unknown.** The C++ GUI thread calls
    `dfx_dsp_.loadPreset`, `setEqBandFrequency`, etc. while the audio thread is inside
    `processAudio`. Whatever internal lock makes that safe is a real-time hazard that the Rust
    design (triple-buffered params, §18.4) removes — but only if the DSP itself is rewritten
    rather than FFI-wrapped. If you wrap the existing C++ DSP over FFI, the hazard comes with it.
16. **Localisation codes `ua` (should be `uk`) and `ba` (should be `bs`)** (`FxLanguage.cpp:25`,
    `.cpp:2438`, `:2430`) are wrong per ISO 639-1. Fluent/ICU tooling will reject or misroute
    them. Decide whether to fix the codes (and migrate the `language` setting) or keep them.
17. **Preset identity is the embedded name**, so two `.fac` files with the same internal name —
    one factory, one user — collide in the autosave namespace and in `isPresetNameValid`. The
    Windows code tolerates this by accident. Define the invariant explicitly in Rust.
18. **`FxModel::getPreset(i)` returning a default-constructed `Preset`** on an out-of-range index
    (`FxModel.cpp:107-115`) is relied upon in at least one place (`FxView.cpp:176-179` checks for
    an empty name). Rust's `Option<&Preset>` is the right shape; audit every call site rather than
    reproducing the sentinel.
