# 12 — Audio Device Plumbing (`audiopassthru`) and its Linux/PipeWire Replacement

> **Scope.** This document reverse-engineers the entire `audiopassthru/` subsystem of the Windows
> FxSound application and specifies, in full implementation detail, the Linux (PipeWire + Rust)
> replacement. `audiopassthru/` is flagged by the project `CLAUDE.md` as the **highest-risk module**
> in the tree ("bugs can affect system audio for all users, not just this app"). The Linux design
> below is written with that in mind: every operation that mutates *global* audio state (the default
> sink, another node's volume, the graph topology) is called out explicitly and given a
> save/restore contract.
>
> **Every number in this document is cited as `path:line`.** Paths are relative to the repository
> root `/home/blackixxce/Загрузки/fxsound-app-main`.

---

## Table of contents

1. [What the subsystem is](#1-what-the-subsystem-is)
2. [Public API surface](#2-public-api-surface)
3. [The handle: `sndDevicesHdlType`](#3-the-handle-snddeviceshdltype)
4. [Constants, limits and magic numbers](#4-constants-limits-and-magic-numbers)
5. [Device enumeration and identification](#5-device-enumeration-and-identification)
6. [Detecting the FxSound virtual device and making it default](#6-detecting-the-fxsound-virtual-device-and-making-it-default)
7. [Choosing the real output device — the "device rules"](#7-choosing-the-real-output-device--the-device-rules)
8. [Format negotiation and the DFX device format push](#8-format-negotiation-and-the-dfx-device-format-push)
9. [Buffer sizing and the WASAPI shared-mode setup](#9-buffer-sizing-and-the-wasapi-shared-mode-setup)
10. [The capture → process → render loop](#10-the-capture--process--render-loop)
11. [Channel mapping and up-sampling](#11-channel-mapping-and-up-sampling)
12. [Hot-plug, default-device-change and state-change handling](#12-hot-plug-default-device-change-and-state-change-handling)
13. [Volume and mute interaction](#13-volume-and-mute-interaction)
14. [Persisted state (registry)](#14-persisted-state-registry)
15. [Every failure mode the code guards against](#15-every-failure-mode-the-code-guards-against)
16. [Lifecycle / teardown](#16-lifecycle--teardown)
17. [Windows → Linux mapping table](#17-windows--linux-mapping-table)
18. [Linux design: comparison of the three options](#18-linux-design-comparison-of-the-three-options)
19. [Linux design: the chosen architecture in full](#19-linux-design-the-chosen-architecture-in-full)
20. [Exact node properties](#20-exact-node-properties)
21. [Becoming the default sink, politely](#21-becoming-the-default-sink-politely)
22. [Surviving a PipeWire restart](#22-surviving-a-pipewire-restart)
23. [Real-time thread constraints](#23-real-time-thread-constraints)
24. [Latency budget and recommended buffer sizes](#24-latency-budget-and-recommended-buffer-sizes)
25. [Rust module layout, crates and types](#25-rust-module-layout-crates-and-types)
26. [Test plan](#26-test-plan)
27. [Open questions / risks for the Rust port](#open-questions--risks-for-the-rust-port)
28. [Linux input mode: FxSound behind a microphone](#28-linux-input-mode-fxsound-behind-a-microphone)

---

## 1. What the subsystem is

FxSound on Windows does **not** hook into applications and does **not** ship a DSP APO. Instead it
ships a kernel virtual audio driver ("FxSound Audio Enhancer") that presents itself as an ordinary
WASAPI *render* endpoint. `audiopassthru/` is the user-space half of the trick:

```
                        ┌──────────────────────────────────────────────┐
  Every app on the      │   FxSound Audio Enhancer  (virtual endpoint) │
  system renders to ───►│   == "capture device" from our point of view │
  the SYSTEM DEFAULT    └───────────────────┬──────────────────────────┘
  device, which we                          │  WASAPI **loopback** capture
  force to be the                           │  (AUDCLNT_STREAMFLAGS_LOOPBACK)
  virtual endpoint.                         ▼
                        ┌──────────────────────────────────────────────┐
                        │  fCaptureBuf  (float32, interleaved)         │
                        └───────────────────┬──────────────────────────┘
                                            │ channel map / upmix
                                            ▼
                        ┌──────────────────────────────────────────────┐
                        │  fPlaybackBuf → DfxDsp::processAudio() in    │
                        │  place → optional zero-order-hold upsample   │
                        └───────────────────┬──────────────────────────┘
                                            │  WASAPI shared-mode render
                                            ▼
                        ┌──────────────────────────────────────────────┐
                        │  REAL playback endpoint (speakers/HDMI/BT…)  │
                        └──────────────────────────────────────────────┘
```

The three moving parts:

| Layer | Files | Role |
| --- | --- | --- |
| `AudioPassthru` (C++ facade) | `audiopassthru/include/AudioPassthru.h`, `audiopassthru/src/AudioPassthru/AudioPassthru.cpp` | Thin PIMPL wrapper; the only thing the JUCE GUI sees. |
| `AudioPassthruPrivate` | `audiopassthru/include/u_AudioPassthru.h`, `audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp` | Owns the processing thread, the restart policy, the `SoundDevice` list projection. |
| `sndDevices` (C module, PT-handle style) | `audiopassthru/include/sndDevices.h` + 12 `.cpp` under `audiopassthru/src/sndDevices/` | All WASAPI/COM/`IPolicyConfig`/registry work. |

`sndDevices` is written in an old "Power Technology" C style: a `PT_HANDLE*` (`typedef int PT_HANDLE`,
`audiopassthru/include/codedefs.h:148`) is cast to `struct sndDevicesHdlType*`; every function returns
`OKAY` (`0`, `codedefs.h:95`) or `NOT_OKAY` (a macro that in Release **pops a MessageBox**,
`codedefs.h:132`) and reports detail through an out-param `int* ip_status`/`ip_resultFlag`.

> **Note.** The handle is a *static* object, not heap-allocated:
> `sndDevicesHdlType AudioPassthruPrivate::s_sndDevices_;`
> (`audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:34`), with the comment that making it
> non-static caused access violations in `MMDevApi.dll` because the COM callback objects are embedded
> in it. That is a Windows COM-lifetime artefact with **no Linux analogue**.

---

## 2. Public API surface

`audiopassthru/include/AudioPassthru.h:62-81`:

| Method | Line | Semantics |
| --- | --- | --- |
| `AudioPassthru()` / `~AudioPassthru()` | 65–66 | ctor allocates `AudioPassthruPrivate`; dtor kills thread, restores default device, frees. |
| `int init()` | 67 | Calls `sndDevicesInit`; wrapped in `try/catch(...)` returning `NOT_OKAY_NO_BREAK` (=1, `codedefs.h:96`) — `AudioPassthru.cpp:36-43`. |
| `void mute(bool)` | 68 | Sets `mute_`; the processing thread then simply **skips `sndDevicesDoPlayback`** (`AudioPassthruPrivate.cpp:568-572`). |
| `std::vector<SoundDevice> getSoundDevices(bool active_devices = true)` | 69 | Re-enumerates if `checkDeviceChanges()` says so, then projects the handle into `SoundDevice` structs. |
| `int setBufferLength(int msecs)` | 70 | Writes the user buffer size to the registry and kills the processing thread so the next timer tick re-inits with it. **No call site exists in `fxsound/`** — dead API today. |
| `int processTimer()` | 71 | The whole restart state machine. Called every **100 ms** (`fxsound/Source/GUI/FxController.cpp:1735`, `startTimer(100)`; invoked at `FxController.cpp:2061`). |
| `void setDspProcessingModule(DfxDsp*)` | 72 | Injects the DSP engine. |
| `void setAsPlaybackDevice(const SoundDevice)` | 73 | Forwards `sound_device.pwszID` to `setTargetedRealPlaybackDevice` (`AudioPassthru.cpp:57-60`). |
| `void registerCallback(AudioPassthruCallback*)` | 74 | Single **static** callback pointer (`u_AudioPassthru.h:73`). |
| `bool isPlaybackDeviceAvailable()` | 75 | Mirrors `playbackDeviceIsUnavailable`. |
| `bool checkDeviceChanges()` | 76 | Cheap re-enumeration diff. |
| `void restoreDefaultPlaybackDevice()` | 77 | Hands the system default back to a real device. |

`AudioPassthruCallback` has exactly one method: `virtual void onSoundDeviceChange(bool processing) = 0;`
(`AudioPassthru.h:58`). `processing == true` means "we just (re)started the processing thread";
`false` means "a device event fired". The GUI branches on it at
`fxsound/Source/GUI/FxController.cpp:2119-2143`.

### `struct SoundDevice` (`AudioPassthru.h:32-53`)

| Field | Line | Notes |
| --- | --- | --- |
| `IMMDevice *pAllDevices` | 33 | Unused in the projection; always `NULL` in the vector. |
| `bool isCaptureDevice` | 34 | True for the FxSound virtual endpoint (it *is* our capture source). |
| `bool isPlaybackDevice` | 35 | **Never set** by `sndDeviceHandleToSoundDevices` — always `false`. |
| `bool isTargetedRealPlaybackDevice` | 36 | The real endpoint we currently render to. |
| `bool isRealDevice` | 37 | Not the FxSound virtual endpoint. |
| `bool isDFXDevice` | 38 | Is the FxSound virtual endpoint. |
| `bool isUserSelectedPlaybackDevice` | 39 | Compared against an **uninitialised stack buffer** — see §15. |
| `bool isDefaultDevice` | 40 | Current Windows default (`eRender`/`eMultimedia`). |
| `bool isActive` | 41 | `deviceState == DEVICE_STATE_ACTIVE`. |
| `std::wstring pwszID` | 43 | The WASAPI endpoint ID string — **the stable identity**. |
| `std::wstring deviceFriendlyName` | 47 | e.g. `"Speakers (Realtek(R) Audio)"`. |
| `std::wstring deviceDescription` | 48 | e.g. `"Speakers"`. |
| `std::wstring deviceFormFactor` | 51 | One of the 10 strings at `sndDevices_GetAll.cpp:198-208`. |
| `int deviceNumChannel` | 52 | From the mix format. |

---

## 3. The handle: `sndDevicesHdlType`

`audiopassthru/include/sndDevices.h:326-447`. The fields that matter for the port:

| Field | Line | Meaning |
| --- | --- | --- |
| `IMMDevice *pAllDevices[64]` | 343 | One COM object per enumerated render endpoint. |
| `IMMDevice *pCaptureDevice`, `*pPlaybackDevice` | 344–345 | **Aliases** into `pAllDevices[]`, no extra ref (`sndDevicesInit.cpp:191-192`, `:206-209`). |
| `WCHAR pwszID[64][512]` | 347 | Endpoint IDs. |
| `LPWSTR pwszIDRealDevices[64]` | 348 | **Pointers into `pwszID`** for non-DFX devices only. |
| `WCHAR pwszIDPreviousRealDevices[64][512]` | 349 | Snapshot used to detect "a device was just added". |
| `DWORD deviceState[64]` | 350 | `DEVICE_STATE_ACTIVE` / `UNPLUGGED` / … |
| `wchar_t deviceFriendlyName[64][512]`, `deviceDescription[64][512]`, `deviceFormFactor[64][512]` | 352–357 | |
| `int deviceNumChannel[64]` | 356 | |
| `IAudioClient *pAudioClientCapture`, `IAudioCaptureClient *pAudioCaptureLoopback` | 359–360 | Loopback capture pair. |
| `IAudioClient *pAudioClientPlayback`, `IAudioRenderClient *pAudioClientPlaybackRender` | 362–363 | Render pair. |
| `IAudioEndpointVolume *pEndptVolCapture`, `*pEndptVolPlayback` | 361, 364 | Endpoint volume on both sides. |
| `REFERENCE_TIME hnsRequestedDuration{Capture,Playback}` | 370, 372 | What we ask WASAPI for. |
| `REFERENCE_TIME hnsActualDuration{Capture,Playback}` | 371, 373 | What we got back. |
| `WAVEFORMATEX wfxCapture, wfxPlayback, wfxDfxProcessing, wfxRecording` | 375–378 | |
| `UINT32 bufferFrameSize{Capture,Playback}` | 382–383 | From `IAudioClient::GetBufferSize`. |
| `float *fCaptureBuf, *fPlaybackBuf, *fFilePlaybackBuf` | 391–393 | Heap `calloc`'d float32 interleaved. |
| `UINT32 capturedFramesCount, playbackFrameCount, numPlaybackFramesAvailableToFill` | 397–399 | |
| `UINT32 upsampleRatio` | 401 | Integer ratio, zero-order-hold. |
| `int bufferSizeMilliSecs` | 403 | **Average delay**; real buffers are 2× this. |
| `int dfxDeviceNum, defaultDeviceNum, playbackDeviceNum, captureDeviceNum, priorDefaultDeviceNum` | 406–411 | Indices or `SND_DEVICES_DEVICE_NOT_PRESENT` (= `-2`). |
| `int totalNumDevices, numPreviousRealDevices, numRealDevices` | 412–414 | |
| `BOOL ignoreDeviceCallbacks, ignoreVolumeCallbacks` | 421–422 | Re-entrancy guards. |
| `int playbackIsActive` | 424 | `SND_DEVICES_PLAYBACK_IS_{STOPPED,ACTIVE}`. |
| `int playbackStreamIsTemporarilyPaused` | 425 | Idle power-saving state. |
| `wchar_t savedPlaybackDeviceID[512]`, `float savedPlaybackVolume`, `BOOL savedPlaybackMute`, `BOOL playbackSettingsAreSaved` | 432–435 | **The polite save/restore contract** — see §13. |
| `BOOL playbackDeviceIsUnavailable` | 441 | Latch for `AUDCLNT_E_DEVICE_IN_USE` / `AUDCLNT_E_UNSUPPORTED_FORMAT`. |
| `void (*deviceChangeCallback)()` | 446 | Set to `AudioPassthruPrivate::onDeviceChange` at `AudioPassthruPrivate.cpp:98`. |
| `GUID guidThisApplication` | 341 | `CoCreateGuid` at `sndDevicesInit.cpp:80`; used as the `guidEventContext` so our own volume writes don't recurse. |

---

## 4. Constants, limits and magic numbers

All from `audiopassthru/include/sndDevices.h` unless stated.

| Constant | Value | Line |
| --- | --- | --- |
| `PT_MAX_GENERIC_STRLEN` | `512` | 30 |
| `PT_MAX_PATH_STRLEN` | `1024` | `pt_defs.h:67` |
| `SND_DEVICES_MONO_BUG_DO_NOT_PROCESS` | `IS_TRUE` (=1) | 37 |
| `SND_DEVICES_MONO_BUG_FORCE_SILENCE` | `IS_TRUE` | 38 |
| `SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES` | `IS_TRUE` | 39 |
| `SND_DEVICES_MAX_NUM_DEVICES` | `64` | 48 |
| `SND_DEVICES_DFX_DEVICE_STRING` | `L"FxSound Audio Enhancer"` | 51 |
| `SND_DEVICES_DFX_PREVIOUS_DEVICE_STRING` | `L"DFX Audio Enhancer"` (legacy, unused) | 52 |
| `SND_DEVICES_DFX_DEVICE_DESCRIPTION_STRING` | `L"FxSound Speakers"` (defined, never referenced) | 55 |
| `SND_DEVICES_INIT_FOR_PROCESSING` / `_NO_PROCESSING` | `1` / `2` | 59–60 |
| `SND_DEVICES_START_CAPTURE` / `_STOP_CAPTURE` | `6` / `7` | 62–63 |
| `SND_DEVICES_AUTO_SELECT_DEFAULT_DEVICE_OFF` / `_ON` | `0` / `1` | 66–67 |
| `SND_DEVICES_PLAYBACK_IS_STOPPED` / `_ACTIVE` | `0` / `1` | 70–71 |
| `SND_DEVICES_DEVICE_OPERATION_COMPLETED` | `0` | 74 |
| `SND_DEVICES_DEVICE_NOT_PRESENT` | `-2` | 76 |
| `SND_DEVICES_DEFAULT_CHANGE_WAIT_TIME` | `1500` ms (currently commented out at both call sites, `sndDevicesImplementDeviceRules.cpp:449`) | 181 |
| `SND_DEVICES_CALLBACK_TIME_WINDOW`, `SND_DEVICES_CALLBACK_WAIT_TIME` | `0`, `0` | 177, 179 |
| `SND_DEVICES_MAX_SAMP_FREQ` | `192000` | 189 |
| `SND_DEVICES_MIN_NUM_CHANS` | `2` | 190 |
| `SND_DEVICES_MAX_NUM_CHANS` | `8` | 191 |
| `..._DEFAULT_SIZE_MILLI_SECS_32BIT_OS_32BIT_CPU` | `80` | 194 |
| `..._DEFAULT_SIZE_MILLI_SECS_32BIT_VISTA_32BIT_CPU` | `100` | 195 |
| `..._DEFAULT_SIZE_MILLI_SECS_32BIT_VISTA_64BIT_CPU` | `100` | 196 |
| `..._DEFAULT_SIZE_MILLI_SECS_32BIT_OS_64BIT_CPU` | `60` | 197 |
| `..._DEFAULT_SIZE_MILLI_SECS_64BIT_OS` | `40` | 198 |
| `SND_DEVICES_CAPTURE_BUFFER_DEFAULT_SIZE_MILLI_SECS` | **`80`** (aliases the 32-bit value even on x64!) | 199 |
| `SND_DEVICES_CAPTURE_BUFFER_MIN_SIZE_MILLI_SECS` | `10` | 200 |
| `SND_DEVICES_CAPTURE_BUFFER_MAX_SIZE_MILLI_SECS` | `100` | 201 |
| `SND_DEVICES_REFTIMES_PER_SEC` | `1.0e7` (100-ns ticks/s) | 202 |
| `SND_DEVICES_MAX_TO_MIN_PLAYBACK_SIZE_RATIO` | `2` | `u_sndDevices.h:23` |
| `DFXG_SND_SERVER_KILL_THREAD_TIMEOUT_MSECS` | `3000` | `AudioPassthruPrivate.cpp:23` |
| `DFXG_SND_SERVER_KILL_THREAD_WAIT_PER_LOOP_MSECS` | `50` | `AudioPassthruPrivate.cpp:24` |
| Initial `savedPlaybackVolume` | `0.25f` | `sndDevicesInit.cpp:111` |
| GUI timer period | `100` ms | `fxsound/Source/GUI/FxController.cpp:1735` |

### Error codes (`sndDevices.h:74-119`) — the exhaustive list

`0` completed · `-1` generic · `-2` not present · `-3` property set failed · `-4` rules not possible ·
`-13` DLL load · `-14` thread requested exit · `-15` thread terminate failed · `-16` instance create ·
`-17` enumerate · `-18` register · `-19` get count · `-20` get ID · `-21` get devices · `-22` get audio
endpoint · `-23` get indexed device · `-24` get padding · `-25` get reg props · `-26` get buffer ·
`-27` release buffer · `-28` null capture client · `-29` null playback client · `-30` null loopback
client · `-31` wait for mutex · `-32` playback render failed · `-33` thread playback failed ·
`-34` device activation failed · `-35` device get format failed · `-36` device set format failed ·
`-37` device open props failed · `-38` icon prop get failed · `-39` device init prop failed ·
`-40` capture device is null · `-41` samp freq not valid · `-42` num chans not valid · `-43` MP3 DLL ·
`-44` set master volume failed · `-45` get master volume failed · `-46` null volume endpoint ·
`-47` set mute failed · `-48` null capture device · `-49` null playback device · `-50` get service
failed · `-54` audio client init failed · `-57` **no valid playback device** · `-58` **ask user to
select playback device**.

Capture/playback loop returns (`sndDevices.h:133-143`): `0` success · `201` capture error ·
`202` capture file write error · `203` playback error · `204` capture forced exit · `205` file playback
forced exit · `206` EOF · `207` capture not possible · `208` playback not possible · `209` no real
devices found.

Device specifier pseudo-enum (`sndDevices.h:146-152`): `100` targeted real playback · `101` virtual
playback DFX · `103` user-selected playback · `104` capture · `105` default · `106` prior default ·
`107` prior playback.

---

## 5. Device enumeration and identification

`sndDevices_GetAll()` — `audiopassthru/src/sndDevices/sndDevices_GetAll.cpp:42-287`.

**Step 0 — release the previous generation.** Every `pAllDevices[i]` is `Release()`d and nulled
(`:76-83`), and `pCaptureDevice`/`pPlaybackDevice` nulled (`:84-85`), because this function is called
repeatedly (up to 20× per re-init, §9) and would otherwise leak COM objects.

**Step 1 — snapshot previous real devices.** If `numRealDevices > 0 && numRealDevices !=
numPreviousRealDevices`, the current real-device IDs are copied into `pwszIDPreviousRealDevices`
(`:88-99`). `numRealDevices` is then reset to 0 (`:100`).

**Step 2 — enumerate.** `CoCreateInstance(MMDeviceEnumerator)` (`:102`), then
`EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE | DEVICE_STATE_UNPLUGGED, …)` (`:106`).
**Only render endpoints.** **Disabled and not-present endpoints are excluded.** Count via
`GetCount` (`:110`); if `<= 0`, the function zeroes `numRealDevices`, `dfxDeviceNum` and
`defaultDeviceNum` and returns (`:117-123`).

**Step 3 — current default.** `GetDefaultAudioEndpoint(eRender, eMultimedia, …)` → `GetId` (`:126-133`).
Note `eMultimedia` (not `eConsole`) is used for *reading* the default, while `eConsole` is used for
*writing* it (§6) — an asymmetry worth keeping in mind.

**Step 4 — pass 1, per device (`:141-267`):**
1. `Item(i, &pAllDevices[i])`.
2. `GetId(&pID)` → `wcscpy_s(pwszID[i], 512, pID)` → `CoTaskMemFree(pID)` (`:151-156`).
3. `OpenPropertyStore(STGM_READ)` (`:159`).
4. `PKEY_Device_FriendlyName` (`:167`). On failure: record state, set `deviceNumChannel[i] = 1`, `continue` (`:168-174`).
5. `PKEY_Device_DeviceDesc` (`:177`). On failure: `continue` (`:178-183`).
6. `PKEY_AudioEndpoint_FormFactor` → string (`:193-216`). The exact mapping:

   | `EndpointFormFactor` | String written |
   | --- | --- |
   | `RemoteNetworkDevice` | `L"NetworkDevice"` |
   | `Speakers` | `L"Speakers"` |
   | `LineLevel` | `L"LineLevel"` |
   | `Headphones` | `L"Headphones"` |
   | `Microphone` | `L"Microphone"` |
   | `Headset` | `L"Headset"` |
   | `Handset` | `L"Handset"` |
   | `UnknownDigitalPassthrough` | `L"DigitalPassthrough"` |
   | `SPDIF` | `L"SPDIF"` |
   | `DigitalAudioDisplayDevice` | `L"HDMI"` |
   | anything else / not `VT_UI4` | `L"Unknown"` |

7. Channel count via `sndDevicesGetFormatFromID` (`:219`), which activates a **throw-away
   `IAudioClient` per device** and calls `GetMixFormat` (`sndDevicesGet.cpp:431-459`). On failure
   `deviceNumChannel[i] = 0` (`:229`).
8. Default match by ID string (`:232-233`).
9. **DFX match by substring**, not equality: `pstrCalcLocationOfStrInStr_Wide(friendlyName,
   L"FxSound Audio Enhancer", 0, …)` (`:238-247`). The comment at `:236-237` explains why: Windows may
   prefix a duplicate device name with `"2- "`.
10. `GetState(&deviceState[i])` (`:249`).

If either name property came back `NULL`, the device is labelled `L"Unknown"` / `L"Unknown"` and
`deviceNumChannel[i] = 1` (`:260-262`).

**Step 5 — pass 2 (`:273-282`):** every index `!= dfxDeviceNum` becomes a "real device":
`pwszIDRealDevices[n] = pwszID[i]` (a *pointer alias*), same for the friendly-name and description
arrays, `numRealDevices++`. Two-pass is required because the DFX index is only known after pass 1.

### Cheap change detection

`sndCheckDeviceChanges()` — `sndDevicesReInit.cpp:344-432`. Re-enumerates with the same filter
(`:366`), compares `GetCount` to `totalNumDevices` (`:374`) and, if equal, compares every ID and
**every device state** (`:398-418`). Any difference sets `*bp_deviceChanged = TRUE` **and**
`stopAudioCaptureAndPlaybackLoop = 1` — i.e. it tears down the processing thread as a side effect of
a query. `AudioPassthruPrivate::getSoundDevices` calls it first (`AudioPassthruPrivate.cpp:134`).

### Projection into `SoundDevice`

`AudioPassthruPrivate::sndDeviceHandleToSoundDevices` — `AudioPassthruPrivate.cpp:147-241`.
Skips entries with empty ID (`:174`), skips non-`DEVICE_STATE_ACTIVE` when `active_devices` is true
(`:179-182`), and **skips every mono device** because `SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES` is
`IS_TRUE` (`:200-203`). `isRealDevice` is an O(n²) scan against `pwszIDRealDevices` (`:207-213`).

---

## 6. Detecting the FxSound virtual device and making it default

**Detection** is purely by friendly-name substring `"FxSound Audio Enhancer"`
(`sndDevices.h:51`, matched at `sndDevices_GetAll.cpp:238`). There is no GUID, no vendor ID, no
device-interface class check. The GUI repeats the same substring test independently at
`fxsound/Source/GUI/FxController.cpp:1504` to set `dfx_enabled_`.

**"Is it enabled?"** is inferred from "can we read its friendly name":
`sndDevicesReInit.cpp:100-129` loops `Sleep(50)` → `sndDevices_GetAll` → `sndDevicesGetFriendlyName(…,
SND_DEVICES_VIRTUAL_PLAYBACK_DFX, …)` until the result flag is `SND_DEVICES_DEVICE_OPERATION_COMPLETED`.
If `loopCount > 20` (**20 × 50 ms = 1 s**, `:116`) and we are in
`SND_DEVICES_INIT_FOR_PROCESSING` mode, it sets `*ipDfxDeviceEnabledFlag = IS_FALSE` and returns
`OKAY` (`:118-122`).

**Making it the default** uses the *undocumented* `IPolicyConfigVista` COM interface shipped in
`audiopassthru/include/PolicyConfig.h` (interface IID `568b9108-44bf-40b4-9006-86afe5b5a620`, class
CLSID `294935CE-F637-4E7C-A41B-AB255460B862`, `PolicyConfig.h:111-114`; the Win7 variant is
`f8679f50-850a-41cf-9c72-430f290290c8` / `870af99c-171d-4f9e-af0d-e63df40c2bc9`, `PolicyConfig.h:23-26`).

`sndDevicesSetDeviceType(…, SND_DEVICES_DEFAULT, id, …)` — `sndDevicesSet.cpp:107-126`:

```cpp
ERole reserved = eConsole;                               // sndDevicesSet.cpp:60
CoCreateInstance(__uuidof(CPolicyConfigVistaClient), …); // :108-109
pPolicyConfig->SetDefaultEndpoint(pwszID[idx], reserved);// :112
priorDefaultDeviceNum = defaultDeviceNum;                // :116
defaultDeviceNum      = device_index_num;                // :117
```

Called from `sndDevicesImplementDeviceRules.cpp:441-452` with the **capture device ID** (= the DFX
device) whenever `defaultSelectionAutoMode == SND_DEVICES_AUTO_SELECT_DEFAULT_DEVICE_ON` — which is
always, in practice (`sndDevicesImplementDeviceRules.cpp:81`, "In current implementation this is
always on"; `sndDevicesGetDefaultDeviceSelectionMode` only returns OFF if the registry value is the
literal string `L"off"`, `sndDevicesGet.cpp:655-658`).

`IPolicyConfigVista::SetEndpointVisibility` is also wrapped (`sndDevicesSet.cpp:347`, `:358`) to
enable/disable an endpoint, but the **disable path is deliberately dead**: see the commented-out block
at `AudioPassthruPrivate.cpp:77-81`, "NOTE: FOR NOW WE DON'T DO THE DISABLE BECAUSE THIS CAN CAUSE
PROBLEMS".

---

## 7. Choosing the real output device — the "device rules"

`sndDevicesImplementDeviceRules()` — `audiopassthru/src/sndDevices/sndDevicesImplementDeviceRules.cpp:44-475`.
This is the single most behaviour-defining function in the module. Reproduce it exactly.

Inputs read from the registry up front (`:111-143`): `user_selected_playback`, `most_recent_default`,
`prior_default`, `original_default`, `most_recent_playback`. Each is turned into an index with
`sndDevices_UtilsGetIndexFromID` (returns `-2` if that ID is not among the currently-enumerated
devices, `sndDevices_Utils.cpp:107-138`).

```
 numRealDevices <= 0 ────────────────────────────────► return OKAY (nothing to do)   :97-101

 most_recent_default == ""  (FIRST RUN AFTER INSTALL)                                :146
   └─ current default != DFX ?
        ├─ yes: write original_default = most_recent_default = current default       :156-159
        │        playbackDeviceNum = current default;  goto PlaybackDeviceIsSelected :162-164
        └─ no : fall through (repair path)                                           :166-167

 numRealDevices == 1 ──► playbackDeviceNum = that device (NO goto — mono check runs) :172-183

 userSelectedPlaybackNum present ──► use it; goto PlaybackDeviceIsSelected           :188-193
     (never set in the current implementation — see the comment at :76)

 A DEVICE WAS JUST ADDED  (numRealDevices > numPreviousRealDevices > 0)              :197
   └─ scan for the first real ID absent from pwszIDPreviousRealDevices,
      skipping devices with < 2 channels                                             :200-221
      playbackDeviceNum = it;  WritePreviousDefault = 1;  goto …                     :226-231

 current default != DFX ──► playbackDeviceNum = current default;
                            WritePreviousDefault = 1                                 :237-249

 else (DFX IS already the default) ──► first ACTIVE of, in order:                    :257-267
        mostRecentPlayback → mostRecentDefault → priorDefault → originalDefault
      none active ──► first real device (wcpp_RealDeviceIDs[0])                      :273-284
```

`PlaybackDeviceIsSelected:` (`:290`) — the **mono guard** (`SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES`):

* If the chosen device has 1 channel (`:300`), retry `mostRecentPlaybackID`.
* If that is also mono: count mono devices (`sndDevicesGetNumMonoDevices`, `sndDevicesGet.cpp:776-806`),
  compute `iNumNonMonoDevices = numRealDevices - iNumMonoDevices` (`:312`).
  * `>= 1` → `*ip_resultFlag = SND_DEVICES_ASK_USER_SELECT_PLAYBACK_DEVICE` (`-58`) (`:318`)
  * `0` → `*ip_resultFlag = SND_DEVICES_NO_VALID_PLAYBACK_DEVICE` (`-57`) (`:324`)
  * return `OKAY` in both cases (`:327`).

Then (`:340-381`):
* `playbackDeviceNum == -2` → `SND_DEVICES_DEVICE_NOT_PRESENT`.
* `pPlaybackDevice = pAllDevices[playbackDeviceNum]`; `NULL` → `NOT_OKAY`.
* Write `most_recent_playback = <chosen ID>` (`:364`).
* If `WritePreviousDefault`: `most_recent_default = <chosen ID>`, `prior_default = <old
  most_recent_playback>` (`:371-376`).

Then §8 (format), then the default-device push (§6), then
`pCaptureDevice = pAllDevices[captureDeviceNum]` (`:455`) and
`upsampleRatio = playbackSamplingFrequency / captureSamplingFrequency` (integer division; `1` if
either is 0) (`:463-468`).

### The GUI's own selection layer

The GUI keeps a parallel, *friendly-name keyed* preference list (`FxSound::DeviceConfig`,
`fxsound/Source/Utils/Settings/DeviceConfig.h:27-48`) and sorts active outputs by it
(`FxController::sortByDeviceConfigPriority`, `fxsound/Source/GUI/FxController.cpp:1714-1725`). It
filters to `isRealDevice && isActive && deviceNumChannel >= 2`
(`FxController.cpp:1494`, `:1548`, `:1672`). Selecting an output calls
`audio_passthru_->setAsPlaybackDevice(...)` (`FxController.cpp:1155`), which — note — does **not** set
`playbackDeviceNum`; it sets that device as the **Windows default** (`AudioPassthruPrivate.cpp:657`),
and the rules above then pick it up on the next re-init because "current default != DFX".

---

## 8. Format negotiation and the DFX device format push

**Playback format** = whatever `IAudioClient::GetMixFormat()` returns for the real endpoint
(`sndDevicesSetupDevices.cpp:385`, stored at `:392`). Always 32-bit float in shared mode — the code
relies on this without checking (`sndDevicesSetupDevices.cpp:115-117` comment; the capture and render
buffers are cast straight to `float*` at `sndDevicesDoCapture.cpp:180` and
`sndDevicesDoPlayback.cpp:112`).

**Capture (DFX) format is *pushed*, not negotiated** — `sndDevicesImplementDeviceRules.cpp:406-438`:

```
numCaptureChannels = playbackFormat.nChannels
if numCaptureChannels < 2           -> 2       (SND_DEVICES_MIN_NUM_CHANS)   :409-410
if numCaptureChannels == 4          -> 6       "quad: capture 5.1, fill backs":413-414
if numCaptureChannels > 8           -> 8       (SND_DEVICES_MAX_NUM_CHANS)   :416-417

if (playbackRate % 48000 == 0)  captureRate = 48000   (flag SND_DEVICES_DFX_SAMP_FREQ_48 = 1) :419-423
else                            captureRate = 44100   (flag ..._44_1 = 0)                     :425-429
wfxDfxProcessing = playbackFormat; wfxDfxProcessing.nSamplesPerSec = captureRate              :393, :438
```

`sndDevicesSetDfxDeviceSampleRateAndChannels` — `sndDevicesSet.cpp:450-524` — then rewrites the DFX
endpoint's *device format* through `IPolicyConfigVista::GetDeviceFormat(id, FALSE /*current*/,
&pwfx)` / `SetDeviceFormat(id, pwfx, NULL)` (`:483`, `:509`), filling:

| Field | Value | Line |
| --- | --- | --- |
| `nSamplesPerSec` | `44100` or `48000` | 491, 493 |
| `nChannels` | `iNumChannels` (validated `2..8` at `:470-471`) | 495 |
| `nBlockAlign` | `nChannels * wBitsPerSample / 8` | 497 |
| `nAvgBytesPerSec` | `nSamplesPerSec * nBlockAlign` | 499 |
| `dwChannelMask` (2 ch) | `3` | 502 |
| `dwChannelMask` (6 ch) | **`1551`** — not `63`; the comment at `:504-505` says `63` causes an error | 504 |
| `dwChannelMask` (otherwise) | **`1599`** | 507 |

> `1551 = 0x60F` = FL|FR|FC|LFE|SL|SR; `1599 = 0x63F` = FL|FR|FC|LFE|BL|BR|SL|SR. These are the
> masks the driver actually accepts; they are **not** the textbook 5.1/7.1 masks.

**Playback client format negotiation** — `sndDevicesFinalSetupPlaybackDevice`,
`sndDevicesSetupDevices.cpp:402-535`:

1. Release + re-`Activate` to get a *fresh, uninitialised* `IAudioClient` (`:425-431`) — WASAPI
   forbids re-`Initialize`.
2. `GetMixFormat` on the fresh client (`:452`) — a copied `WAVEFORMATEX` is rejected (`:450`).
3. `IsFormatSupported(AUDCLNT_SHAREMODE_SHARED, pwfx, &pClosestMatch)`:
   * returns `S_FALSE` **and** a closest match → discard `pwfx`, re-activate, `Initialize` with
     `pClosestMatch` (`:466-477`). *"Some devices (e.g. Bluetooth) return a mix format from
     GetMixFormat that their driver then rejects"* (`:460-461`).
   * otherwise → `Initialize` with `pwfx`; if that returns `AUDCLNT_E_UNSUPPORTED_FORMAT` **and** a
     closest match exists, re-activate and retry with it (`:485-494`).
4. On final failure: if `hr == AUDCLNT_E_DEVICE_IN_USE || hr == AUDCLNT_E_UNSUPPORTED_FORMAT`,
   latch `playbackDeviceIsUnavailable = TRUE` (`:500-501`) and return
   `SND_DEVICES_AUDIO_CLIENT_INIT_FAILED` (`-54`).

---

## 9. Buffer sizing and the WASAPI shared-mode setup

`sndDevicesReInit()` — `audiopassthru/src/sndDevices/sndDevicesReInit.cpp:42-342`.

**Buffer length resolution (`:233-258`):**

```
bufferSizeMilliSecs = 80                                  // SND_DEVICES_CAPTURE_BUFFER_DEFAULT…  :233
read HKCU  "user_buffer_size"     -> if non-empty, parse  // :236-242
else read HKLM "default_buffer_size" -> if non-empty, parse// :246-253
if (< 10 || > 100) bufferSizeMilliSecs = 80               // :257-258
```

`sndDevicesGetRecommendedBufferSizeMilliSecs` (`sndDevicesGet.cpp:709-753`) would return `40` on a
64-bit OS, `60` on 32-bit-OS/64-bit-CPU, `100` on Vista — but it is **not used by `ReInit`**; the
compiled default stays `80` ms.

**Requested WASAPI durations (`:261-262`):**

```
hnsRequestedDurationCapture  = bufferSizeMilliSecs * 1.0e7 / 500.0
hnsRequestedDurationPlayback = bufferSizeMilliSecs * 1.0e7 / 500.0
```

`/500` (not `/1000`) because — per the comment at `:260` — `bufferSizeMilliSecs` is the **average bulk
delay** and the real ring is **twice** that. So 80 ms → `1 600 000` reftime ticks = **160 ms** of
WASAPI buffer, on both sides.

**Heap allocation (`:264-311`):**

```
captureBufferChannelsForAllocation =
      (wfxPlayback.nChannels > wfxCapture.nChannels && upsampleRatio > 1)
          ? wfxPlayback.nChannels : wfxCapture.nChannels                       :271-274
captureAllocSize  = wfxCapture.nSamplesPerSec  * captureBufferChannelsForAllocation
                    * bufferSizeMilliSecs * 1.001 / 500.0                      :276
playbackAllocSize = wfxPlayback.nSamplesPerSec * wfxPlayback.nChannels
                    * bufferSizeMilliSecs * 1.001 / 500.0                      :277
```

(`1.001` = rounding slop, `sndDevices.h:389`.) Buffers are only re-`calloc`'d when the size changed
(`:279-308`); `fFilePlaybackBuf` is allocated the same size as `fPlaybackBuf` but is **never used** by
the passthru path.

**Capture client init** — `sndDevicesFinalSetupCaptureDevice`, `sndDevicesSetupDevices.cpp:136-220`:

```cpp
StreamFlags = (procInfo & OPERATING_SYSTEM_VISTA)
            ? AUDCLNT_STREAMFLAGS_LOOPBACK
            : AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_SESSIONFLAGS_DISPLAY_HIDE;   // :169-173
pAudioClientCapture->Initialize(AUDCLNT_SHAREMODE_SHARED, StreamFlags,
                                hnsRequestedDurationCapture, 0, pwfx, NULL);       // :185-186
GetBufferSize(&bufferFrameSizeCapture);                                            // :195
GetService(IID_IAudioCaptureClient, &pAudioCaptureLoopback);                        // :203
hnsActualDurationCapture = 1.0e7 * bufferFrameSizeCapture / wfxCapture.nSamplesPerSec; // :217
```

`AUDCLNT_SESSIONFLAGS_DISPLAY_HIDE` hides our duplicate volume slider in the Windows mixer
(`:172`). **Note: this is `AUDCLNT_SHAREMODE_SHARED` with a *timer-driven* (polling) model — no
event handle is passed (the 6th argument to `Initialize` is `NULL`, and no
`SetEventHandle` call exists anywhere in the tree).** The loop instead polls with `Sleep(1)`
(§10). Exclusive mode is never used.

Playback side: `StreamFlags = 0` on Vista, else `AUDCLNT_SESSIONFLAGS_DISPLAY_HIDE`
(`sndDevicesSetupDevices.cpp:444-448`), then `Initialize(SHARED, …, hnsRequestedDurationPlayback, 0,
fmt, NULL)`, `GetBufferSize(&bufferFrameSizePlayback)` (`:507`),
`numPlaybackFramesAvailableToFill = bufferFrameSizePlayback` (`:515`),
`GetService(IID_IAudioRenderClient, …)` (`:518`),
`hnsActualDurationPlayback = 1.0e7 * bufferFrameSizePlayback / wfxPlayback.nSamplesPerSec` (`:532`).

### Re-init ordering (exact)

1. Reset every index to `-2`, `upsampleRatio = 1`, `playbackIsActive = STOPPED`,
   `stopAudioCaptureAndPlaybackLoop = 0`, clear the silent/no-buffer counters (`:70-90`).
2. `sndDevices_FreeReuseableObjects` (`:95`) — releases the 4 audio clients + all `IMMDevice`s
   (`sndDevicesInit.cpp:182-209`).
3. The 20×50 ms DFX-detection loop (`:99-129`).
4. If `numRealDevices > 0`:
   `sndDevicesImplementDeviceRules` → `sndDevices_ReleaseAllAudioObjects` (`:149`) →
   `sndDevicesInitialSetupCaptureDevice` → `sndDevicesInitialSetupPlaybackDevice` →
   read capture mute → sync volume → apply mute to both → size buffers →
   `sndDevicesFinalSetupCaptureDevice` → `sndDevicesFinalSetupPlaybackDevice` →
   `ignoreDeviceCallbacks = ignoreVolumeCallbacks = FALSE` (`:139-335`).

---

## 10. The capture → process → render loop

### The restart state machine — `AudioPassthruPrivate::processTimer` (`AudioPassthruPrivate.cpp:359-474`)

Runs on the GUI thread every 100 ms.

```
hProcessingThread_ == NULL                      -> b_need_to_start_thread = TRUE       :375-376
hProcessingThread_ == INVALID_HANDLE_VALUE      -> "paused" sentinel:                  :377-383
      if (!playbackDeviceIsUnavailable) hProcessingThread_ = NULL   // retry next tick
otherwise GetExitCodeThread != STILL_ACTIVE     -> CloseHandle, restart                :386-392

if (b_need_to_start_thread):
    i_kill_processing_thread_ = IS_FALSE                                               :398
    sndDevicesReInit(…, &numRealDevices, &DfxDeviceEnabledFlag, &statusFlag)            :401
    if (numRealDevices > 0 && DfxDeviceEnabledFlag == IS_TRUE)
        hProcessingThread_ = CreateThread(…, processingThread, this, 0, &tid)           :443
    else
        if (numRealDevices <= 0 || DfxDeviceEnabledFlag != IS_TRUE)
            playbackDeviceIsUnavailable = TRUE                                          :458-459
        hProcessingThread_ = INVALID_HANDLE_VALUE   // pause retries                     :460
    s_callback_->onSoundDeviceChange(true)                                               :463-464
```

The `INVALID_HANDLE_VALUE` sentinel exists because re-running `sndDevicesReInit` every 100 ms "would
both burn CPU and leak COM objects on every retry" (`:452-457`). It is cleared only by
`onDeviceChange` (`:243-252`), which sets `playbackDeviceIsUnavailable = FALSE` and forwards to the
GUI callback.

`AudioPassthru::processTimer` additionally returns `NOT_OKAY_NO_BREAK` if the playback device is
unavailable (`AudioPassthru.cpp:83-86`).

### The worker — `AudioPassthruPrivate::threadWorker` (`AudioPassthruPrivate.cpp:480-588`)

```cpp
SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);   // :493
sndDevicesStartStopCapture(h, SND_DEVICES_START_CAPTURE);               // :496
while (1) {
    if (i_kill_processing_thread_) goto Kill;                            // :506
    sndDevicesDoCapture(h, &fp_buffer, &numSampleSets, &pwfx, &resultFlag); // :513
    if (resultFlag != SUCCESS) goto Kill;                                // :517
    if (i_kill_processing_thread_) goto Kill;                            // :521
    if (numSampleSets > 0) {
        i_valid_bits = pwfx->wBitsPerSample;                             // :527
        i_check_for_duplicate_buffers = IS_FALSE;                        // :528
        p_dfx_dsp_->setSignalFormat(pwfx->wBitsPerSample, pwfx->nChannels,
                                    pwfx->nSamplesPerSec, i_valid_bits); // :533
        // failure deliberately IGNORED — see :535-538
        if (pwfx->nChannels != 1 || !SND_DEVICES_MONO_BUG_DO_NOT_PROCESS)
            p_dfx_dsp_->processAudio((short int*)fp_buffer,
                                     (short int*)fp_buffer,
                                     numSampleSets, i_check_for_duplicate_buffers); // :551
    }
    if (!mute_) sndDevicesDoPlayback(h, &resultFlag);                    // :568-572
    if (resultFlag != SUCCESS) goto Kill;                                // :576
}
Kill: sndDevicesStartStopCapture(h, SND_DEVICES_STOP_CAPTURE); return OKAY;  // :583-587
```

Notes that matter:
* `processAudio` is declared `short int*` but is **always handed float32** — a deliberate lie
  (`dsp/include/DfxDsp.h:44`; comment at `AudioPassthruPrivate.cpp:548`: *"Format will always be 32 bit
  floating point"*). Processing is **in place**: input and output are the same pointer.
* `setSignalFormat` failure is swallowed because in Release builds it fails on the first few calls
  (`:535-538`).
* `mute(true)` **stops calling the render function entirely** — capture keeps running, the WASAPI
  playback ring drains and then `numFramesQueuedUpToPlay` hits 0 and the stream is `Stop()`ped by the
  idle path (§below).
* The thread wrapper calls `CoInitialize(NULL)` / `CoUninitialize()` around the worker
  (`:598`, `:603`).

### `sndDevicesDoCapture` (`audiopassthru/src/sndDevices/sndDevicesDoCapture.cpp:47-396`)

Outer loop (`:88-265`), once per iteration:

1. `stopAudioCaptureAndPlaybackLoop == 1` → return `SND_DEVICES_CAPTURE_FORCED_EXIT` (`204`) (`:91-96`).
2. **`Sleep(1)`** (`:99`). This is the entire pacing mechanism.
3. Clamp `upsampleRatio` to `1` if `<= 0` or `> 16` (`:102-111`) — added for Bluetooth add/remove
   glitches (`:101`).
4. `pAudioClientCapture->GetCurrentPadding(&numFramesQueuedUpToCapture)` (`:113`).
5. `pAudioClientPlayback->GetCurrentPadding(&numFramesQueuedUpToPlay)` (`:117`).
6. `numFramesQueuedUpToPlayReferencedToCapture = numFramesQueuedUpToPlay / upsampleRatio` (`:120`).
7. `numPlaybackFramesAvailableToFill = bufferFrameSizeCapture - <that>` (`:123`).
8. `halfCaptureBufferSize = bufferFrameSizeCapture / 2`,
   `quarterCaptureBufferSize = bufferFrameSizeCapture / 4` (`:125-126`).
9. **Idle handling** (`:129-157`):
   * playback ring empty → `numDesiredCaptureFrames = halfCaptureBufferSize`,
     `playbackIsActive = STOPPED`; if not already paused, set
     `playbackStreamIsTemporarilyPaused = 1` and call `pAudioClientPlayback->Stop()` *"to allow PC to
     sleep if no audio is playing"* (`:137`).
   * otherwise → `playbackIsActive = ACTIVE`; if paused, clear the flag and `Start()` (`:145-149`);
     then `numDesiredCaptureFrames = (numPlaybackFramesAvailableToFill < halfCaptureBufferSize)
     ? 0 : numPlaybackFramesAvailableToFill - halfCaptureBufferSize` (`:152-156`).
     **The target fill level is therefore "half the capture buffer".**
10. `numDesiredCaptureFrames == 0` → `goto DoneGrabbingFrames` (`:159-160`).
11. Inner packet loop (`:167-254`), **no sleep**: `GetNextPacketSize` → `GetBuffer(&pDataPacketCapture,
    &numCaptureFramesAvailable, &flags, NULL, NULL)` → copy `numCaptureFramesAvailable *
    numCaptureChannels` floats into `fCaptureBuf` at `capturedFramesCount * numCaptureChannels` →
    `ReleaseBuffer` → advance. If `flags & AUDCLNT_BUFFERFLAGS_SILENT`, write **zeros** and set
    `playbackIsActive = STOPPED` (`:186-191`). Exit when `capturedFramesCount >=
    numDesiredCaptureFrames` **or** `packetLength == 0`.
    *(Packets are ~10 ms regardless of rate — 441 frames at 44.1 kHz, `:162-163`.)*
12. Re-read playback padding; if `numFramesQueuedUpToPlay != 0 && capturedFramesCount > 0 &&
    numFramesQueuedUpToPlay/upsampleRatio <= quarterCaptureBufferSize`, break out early to flush
    what we have (`:262-263`) — the **drain-at-quarter rule**.

`DoneGrabbingFrames:` (`:267`) — `playbackFrameCount = upsampleRatio * capturedFramesCount` (`:270`),
channel-map into `fPlaybackBuf` (§11), then publish `*fpp_buffer = fPlaybackBuf`,
`*ip_numSampleSets = capturedFramesCount`, `*pp_wfxDfx = &wfxDfxProcessing` (`:383-387`).
`Exit:` maps `hr == S_OK` → `0`, anything else → `SND_DEVICES_CAPTURE_ERROR` (`201`) (`:390-393`).

> **Bug to not reproduce:** `GetNextPacketSize` is called **twice** per inner iteration
> (`:244` and `:250`) — the second call overwrites the first. Harmless in practice but wrong.

### `sndDevicesDoPlayback` (`audiopassthru/src/sndDevices/sndDevicesDoPlayback.cpp:44-125`)

```
if (upsampleRatio > 1):                                                     :70-90
    copy capturedFramesCount*numPlaybackChannels floats fPlaybackBuf -> fCaptureBuf
    for each frame i, repeat it upsampleRatio times into fPlaybackBuf   // ZERO-ORDER HOLD
GetCurrentPadding(&numFramesQueuedUpToPlay)                                 :93
numPlaybackFramesAvailableToFill = bufferFrameSizePlayback - numFramesQueuedUpToPlay :96
if (playbackFrameCount > numPlaybackFramesAvailableToFill)
    playbackFrameCount = numPlaybackFramesAvailableToFill          // silent truncation :99-100
if (playbackFrameCount > 0):
    pAudioClientPlaybackRender->GetBuffer(playbackFrameCount, &pDataPacketPlayback)    :105
    copy playbackFrameCount*numPlaybackChannels floats                                 :111-115
    ReleaseBuffer(playbackFrameCount, 0)                                               :118
```

`sndDevicesStartStopCapture` (`sndDevicesDoCapture.cpp:403-430`) starts/stops **both** clients
together and toggles `stopAudioCaptureAndPlaybackLoop`.

---

## 11. Channel mapping and up-sampling

`sndDevicesDoCapture.cpp:273-379`. Channel order assumed: 5.1 = `FL FR FC LFE BL BR`; 7.1 =
`FL FR FC LFE BL BR SL SR` (`:278-279`).

| capture ch | playback ch | Mapping | Lines |
| --- | --- | --- | --- |
| n | n | straight interleaved copy | 374–378 |
| 6 | 2 | take `[0],[1]` | 288–297 |
| 6 | 4 | take `[0],[1],[4],[5]` (FL FR BL BR) | 298–311 |
| 6 | 8 | `[0..5]` then **`[4],[5]` again** into SL/SR | 312–333 |
| 2 | 4 | `[0],[1],[0],[1]` | 335–349 |
| any | 1 | **silence** — the whole `fPlaybackBuf` is zeroed, because `SND_DEVICES_MONO_BUG_FORCE_SILENCE` is `IS_TRUE` | 350–372 |

Up-sampling is **integer frame repetition** (`sndDevicesDoPlayback.cpp:79-89`) — a zero-order hold,
i.e. a rectangular-window interpolator with heavy imaging. It only triggers when the real device runs
at an integer multiple of 44.1/48 kHz (e.g. a 96 kHz DAC with a 48 kHz DFX device → ratio 2).

> **Do not port this.** On Linux, run the DSP at the sink's own rate and let PipeWire's sinc
> resampler handle any mismatch. See §19.

---

## 12. Hot-plug, default-device-change and state-change handling

`CsndDevicesMMNotificationClient : public IMMNotificationClient`
(`sndDevices.h:220-262`, implemented in `audiopassthru/src/sndDevices/sndDevicesDeviceCallbacks.cpp`).
Registered at `sndDevicesInit.cpp:154` via `RegisterEndpointNotificationCallback`, unregistered at
`sndDevicesInit.cpp:267`.

| Callback | Lines | Behaviour |
| --- | --- | --- |
| `OnDefaultDeviceChanged(flow, role, id)` | 95–156 | Returns immediately if `dfxDeviceNum == -2` (`:106`) or `id == NULL` (`:109`). If `flow == eRender && role == eMultimedia && !ignoreDeviceCallbacks`: **if the new default IS our DFX device, return `S_OK` without doing anything** (`:122-123`); otherwise `stopAudioCaptureAndPlaybackLoop = 1` (`:130`). Then, if the new default is already `defaultDeviceNum`, return (`:134-139`); else rescan `pwszID[]` for the new index (`:141-150`) and fire `deviceChangeCallback()` (`:152-153`). |
| `OnDeviceAdded(id)` | 158–181 | If `!ignoreDeviceCallbacks` → kill flag; always fire `deviceChangeCallback()`. Comment `:169`: these only fire for devices whose driver was not already installed. |
| `OnDeviceRemoved(id)` | 183–206 | Same. |
| `OnDeviceStateChanged(id, state)` | 208–265 | **De-duplicates**: if `(id == lastDeviceAddCallbackGuid) && (state == lastDeviceAddCallbackGuidtype)` → ignore (`:241-245`). Comments record that connecting a device fires ≈10 times and disconnecting ≈12 times (`:225`, `:231`). Then records the new `(id, state)`, kill flag if `!ignoreDeviceCallbacks`, fire `deviceChangeCallback()`. |
| `OnPropertyValueChanged(id, key)` | 271–309 | **Entirely disabled.** The body is commented out (`:282-304`) because on Win8 this fires for *mute* changes and caused a feedback problem (`:267-270`). |

`AudioPassthruPrivate::onDeviceChange` (`AudioPassthruPrivate.cpp:243-252`) clears
`playbackDeviceIsUnavailable` and calls `s_callback_->onSoundDeviceChange(false)`.

GUI side (`fxsound/Source/GUI/FxController.cpp:2119-2143`): ignores events from a different WTS
session (`:2121-2122`), takes a `ScopedLock`, and dispatches to `selectProcessingOutput`
(timer running) or `syncOutputWithSystemDefault` (timer stopped).

---

## 13. Volume and mute interaction

This is the most delicate and most "global-state-mutating" part of the module. Two
`IAudioEndpointVolumeCallback` implementations (`sndDevices.h:265-323`, implemented in
`sndDevicesVolCallbacks.cpp`) keep the DFX endpoint and the real endpoint's volumes **mirrored**:

* Capture (DFX) callback `OnNotify` — `sndDevicesVolCallbacks.cpp:97-148`: if
  `ignoreVolumeCallbacks` → return (`:109`); if `pNotify->guidEventContext == guidThisApplication`
  → **ignore our own writes** (`:118`); else read DFX scalar volume and write it to the playback
  endpoint (`:126-130`), read DFX mute and write it to playback (`:133-138`).
* Playback callback `OnNotify` — `:204-267`: the mirror image, **plus** it updates
  `savedPlaybackVolume` / `savedPlaybackMute` (`:249-255`) — because a change we did not make is by
  definition the user's own setting for that device.

**On re-init** (`sndDevicesReInit.cpp:172-230`), the order is deliberate and documented:
1. Read the **capture (DFX) mute first**, before any volume write, because *"Some drivers clear a
   device's mute flag as a side effect of a volume-level write"* (`:173-175`).
2. `GetMasterVolumeLevelScalar` on the DFX endpoint (`:196`).
3. `SetMasterVolumeLevelScalar(captureVolSetting, &guidThisApplication)` on the **real playback
   endpoint** (`:206`) — the DFX level is authoritative, so "a device sitting at full volume does not
   suddenly play at full volume the moment it is selected" (`:191-192`).
4. Re-assert the mute on both endpoints (`:216`, `:224`), accepting `S_FALSE` as success (`:217`,
   `:225`).

**The save/restore contract** — `sndDevices_RestoreSavedPlaybackDeviceSettings`
(`sndDevicesSetupDevices.cpp:232-278`):

* Saved in `sndDevicesInitialSetupPlaybackDevice` (`:343-370`) *before this app touches the device*,
  and only if we don't already hold settings for that device (`:359`).
* When switching **away** from a device: restore with `b_never_raise_volume = FALSE` (`:352`) — the
  device is about to go silent, a raise is safe.
* When handing control **back** (`sndDevicesRestoreDefaultDevice`, `:580`):
  `b_never_raise_volume = TRUE` — clamp `targetVolume` to `min(saved, current)` (`:265-271`), because
  restoring a *higher* level at the moment the user starts hearing that device would step the volume
  up at exactly the wrong time.
* `playbackSettingsAreSaved` is deliberately **left set** after the hand-back (`:576-579`).
* Initial values: `savedPlaybackDeviceID = L""`, `savedPlaybackVolume = 0.25f`,
  `savedPlaybackMute = FALSE`, `playbackSettingsAreSaved = FALSE` (`sndDevicesInit.cpp:110-113`);
  these survive re-inits and are only reset in the one-time init.
* On shutdown, `ignoreVolumeCallbacks = TRUE` is set **before** `sndDevicesRestoreDefaultDevice`
  so the DFX driver's volume-state reset does not propagate to the real speakers
  (`AudioPassthruPrivate.cpp:65-70`).

`AudioPassthru::mute()` is a *different* concept: it only gates the render call
(`AudioPassthruPrivate.cpp:568`). The GUI uses it for system suspend/resume
(`fxsound/Source/GUI/FxController.cpp:2145-2159`) and for "output disconnected" (`:1167`).

---

## 14. Persisted state (registry)

Value names (`sndDevices.h:159-170`):

| Name | Hive | Purpose |
| --- | --- | --- |
| `devices` | (path component) | Subkey holding all of the below |
| `original_default` | HKCU | Default endpoint at first run after install |
| `most_recent_default` | HKCU | |
| `prior_default` | HKCU | |
| `most_recent_playback` | HKCU | Last real device we rendered to |
| `user_selected_playback` | HKCU | Never written by current code |
| `default_device_mode` | HKCU | `"on"` / `"off"` |
| `user_buffer_size` | HKCU | ms, string |
| `dfx_guid` | HKLM | Written by the installer; guards the disable path |
| `default_buffer_size` | HKLM | ms, string |

Path construction (`audiopassthru/src/sndDevices/sndDevicesReg.cpp:53-65`, `:95-107`):

```
HKCU:  SOFTWARE\DFX\13\23\devices\<value>     // DFXP_REGISTRY_TOP_WIDE="SOFTWARE" (dfxpdefs.h:40)
HKLM:  SOFTWARE\DFX\23\devices\<value>        // product "DFX" (sndDevicesReg.cpp:33)
                                              // (int)DFX_VERSION = 13  (sndDevicesReg.cpp:35)
                                              // DFXP_VENDOR_CODE_UNIVERSAL = 23 (:34)
```

A missing or empty value reads back as `L""` (`sndDevicesReg.cpp:125-128`). The GUI wipes
`HKCU\Software\DFX` wholesale on version change (`fxsound/Source/GUI/FxController.cpp:717`).

---

## 15. Every failure mode the code guards against

| # | Condition | Detection | Response |
| --- | --- | --- | --- |
| 1 | No render endpoints at all | `GetCount() <= 0` | zero all indices, return OK (`sndDevices_GetAll.cpp:117-123`) |
| 2 | `CoCreateInstance(MMDeviceEnumerator)` fails | `FAILED(hr)` | `-16` `INSTANCE_CREATE_FAILED` (`sndDevices_GetAll.cpp:103`) |
| 3 | `EnumAudioEndpoints` fails | | `-17` (`:107`) |
| 4 | `GetCount` fails | | `-19` (`:111`) |
| 5 | `GetDefaultAudioEndpoint` fails | | `-22` (`:127`) |
| 6 | `Item(i)` fails | | free default ID, `-23` (`:144-148`) |
| 7 | `GetId` fails | | `-20` (`:154`) |
| 8 | `OpenPropertyStore` fails | | `-16` (`:160-164`) |
| 9 | `PKEY_Device_FriendlyName` missing | | `deviceNumChannel = 1`, skip device (`:168-174`) |
| 10 | `PKEY_Device_DeviceDesc` missing | | skip device (`:178-183`) |
| 11 | FormFactor missing / wrong vt | | `L"Unknown"` (`:212-215`) |
| 12 | Per-device `GetMixFormat` fails | `resultFlag != 0` | `deviceNumChannel = 0` (`:229`) |
| 13 | Names are `NULL` pointers | | `L"Unknown"`, 1 channel (`:260-262`) |
| 14 | **FxSound driver not installed / disabled** | 20 × 50 ms poll times out | `DfxDeviceEnabledFlag = IS_FALSE`; `processTimer` refuses to start the thread and latches the pause sentinel (`sndDevicesReInit.cpp:116-122`, `AudioPassthruPrivate.cpp:441-461`); GUI shows `FxDeviceErrorMessage` and quits (`FxController.cpp:729-736`, `:1630-1640`) |
| 15 | Only mono real device(s) | channel count == 1 after fallbacks | `-57 NO_VALID_PLAYBACK_DEVICE` (`sndDevicesImplementDeviceRules.cpp:324`) |
| 16 | Chosen device mono but non-mono ones exist | | `-58 ASK_USER_SELECT_PLAYBACK_DEVICE` (`:318`) |
| 17 | Mono *playback* reached anyway | `numPlaybackChannels == 1` | **write silence** (`sndDevicesDoCapture.cpp:353-358`) |
| 18 | Mono *capture* format | `pwfx->nChannels == 1` | skip DSP entirely (`AudioPassthruPrivate.cpp:546`) |
| 19 | No playback device assignable | `playbackDeviceNum == -2` | `-2` (`sndDevicesImplementDeviceRules.cpp:340-344`) |
| 20 | `pPlaybackDevice == NULL` | | `NOT_OKAY` (`:348-349`) |
| 21 | DFX device index missing at rules time | | `-2` (`:398-402`) |
| 22 | `pCaptureDevice == NULL` | | `-40` (`:457-461`) |
| 23 | `Activate(IAudioClient)` fails | | `-34` (`sndDevicesSetupDevices.cpp:76-80`, `:311-315`, `:427-431`) |
| 24 | Audio client came back `NULL` | | `-28` / `-29` (`:83-86`, `:317-321`) |
| 25 | `Activate(IAudioEndpointVolume)` fails | | `-34` (`:92-96`, `:327-331`) |
| 26 | Endpoint volume `NULL` | | `-46` (`:98-102`, `:333-337`) |
| 27 | `RegisterControlChangeNotify` fails | | release + `-18` (`:106-112`, `:374-380`) |
| 28 | `GetMixFormat` fails / `NULL` | | `-35` (`:120-124`, `:178-182`, `:386-390`, `:453-457`) |
| 29 | **Device rejects its own mix format** (Bluetooth) | `IsFormatSupported == S_FALSE` or `AUDCLNT_E_UNSUPPORTED_FORMAT` | retry with `pClosestMatch` after re-`Activate` (`:466-495`) |
| 30 | **Device in use / format unsupported, terminally** | `AUDCLNT_E_DEVICE_IN_USE \|\| AUDCLNT_E_UNSUPPORTED_FORMAT` | `playbackDeviceIsUnavailable = TRUE`, `-54` (`:498-504`); GUI surfaces an output error (`FxController.cpp:1617-1622`, `FxView.cpp:131`) |
| 31 | `IAudioClient::Initialize` fails (capture) | | `-39` (`:188-192`) |
| 32 | `GetBufferSize` fails | | `-26` (`:196-200`, `:508-512`) |
| 33 | `GetService` fails | | `-50` (`:204-208`, `:519-523`) |
| 34 | Loopback / render client `NULL` | | `-30` / `-32` (`:210-214`, `:525-529`) |
| 35 | Capture `GetMute` / `GetMasterVolumeLevelScalar` fails | `hr != S_OK` | `-47` / `-45`, `DfxDeviceEnabledFlag = IS_FALSE` (`sndDevicesReInit.cpp:179-202`) |
| 36 | `SetMasterVolumeLevelScalar` / `SetMute` fails | `hr` not `S_OK`/`S_FALSE` | `-44` / `-47` (`:206-230`) |
| 37 | `calloc` returns `NULL` | | `NOT_OKAY` (`:310-311`) |
| 38 | `upsampleRatio` out of `[1,16]` | | clamp to `1`, trace (`sndDevicesDoCapture.cpp:102-111`) |
| 39 | `GetCurrentPadding` / `GetBuffer` / `ReleaseBuffer` fail mid-loop | `FAILED(hr)` → `goto Exit` | `201 CAPTURE_ERROR` → worker exits → timer re-inits (`:114`, `:118`, `:178`, `:240`, `:245`, `:251`, `:258`) |
| 40 | Render `GetBuffer` / `ReleaseBuffer` fail | | `-32` / `-27` (`sndDevicesDoPlayback.cpp:106`, `:119`) |
| 41 | `playbackFrameCount` exceeds free space | | silent truncation (`sndDevicesDoPlayback.cpp:99-100`) — **audio is dropped, no log** |
| 42 | Silent buffer from the driver | `AUDCLNT_BUFFERFLAGS_SILENT` | write zeros, mark playback stopped (`sndDevicesDoCapture.cpp:186-191`) |
| 43 | Stop requested mid-capture | `stopAudioCaptureAndPlaybackLoop` | `204 CAPTURE_FORCED_EXIT` (`:91-96`) |
| 44 | Processing thread will not die | 3000 ms elapsed in 50 ms steps | `*ip_timed_out = IS_TRUE`; dtor abandons cleanup (`AudioPassthruPrivate.cpp:302-318`, `:62-63`) |
| 45 | `init()` throws | `catch (...)` | `NOT_OKAY_NO_BREAK` (`AudioPassthru.cpp:40-43`); GUI alerts and quits (`FxController.cpp:704-711`) |
| 46 | `SetDeviceFormat` on DFX fails | | `-36`, frees `pwfx`, releases policy config (`sndDevicesSet.cpp:510-516`) |
| 47 | Channel count outside `[2,8]` for the DFX push | | `NOT_OKAY` (`sndDevicesSet.cpp:470-471`) |
| 48 | `SetValue` on a property store fails | usually no admin rights | `-3 PROPERTY_SET_FAILED` (`sndDevicesSet.cpp:185`, `:246`) |
| 49 | Running on XP | `procInfo & OPERATING_SYSTEM_XP` | refuse endpoint enable/disable, `-3` (`sndDevicesSet.cpp:334-338`) |
| 50 | Device event storm | duplicate `(id, state)` pairs | de-dup (`sndDevicesDeviceCallbacks.cpp:241-245`) |
| 51 | Our own volume write echoing back | `guidEventContext` compare | ignore (`sndDevicesVolCallbacks.cpp:118`, `:226`) |
| 52 | Device change while paused | `playbackDeviceIsUnavailable` latch | cleared by `onDeviceChange`, retry next tick (`AudioPassthruPrivate.cpp:246`, `:381-382`) |

**Two real defects to fix rather than port:**

* `AudioPassthruPrivate.cpp:215` compares `sound_device.pwszID` against
  `wcp_user_seleted_playback_device_guid`, a **stack buffer that is never written** (declared `:150`,
  and the `sndDevicesGetID(SND_DEVICES_USER_SELECTED_PLAYBACK_DEVICE, …)` call that would fill it is
  absent from the `if` chain at `:164-167`). `isUserSelectedPlaybackDevice` is therefore garbage.
* `sndDevicesImplementDeviceRules.cpp:223` uses `i` after the `for` loop at `:200-221` may have run to
  completion, indexing `pwszIDRealDevices[numRealDevices]` — one past the last valid entry.

---

## 16. Lifecycle / teardown

`~AudioPassthruPrivate` (`AudioPassthruPrivate.cpp:49-88`), in order:

1. `killProcessingThread(&i_timed_out)`; bail on failure or timeout (`:58-63`).
2. `ignoreVolumeCallbacks = TRUE` (`:67`).
3. `sndDevicesRestoreDefaultDevice` (`:70`) — restores the real device as system default *and* puts
   its own volume/mute back with the never-raise clamp.
4. (Disabling the virtual device is **commented out**, `:74-81`.)
5. `sndDevicesFree` (`:84`) → `sndDevices_ReleaseAllAudioObjects` (unregisters both volume callbacks,
   releases both `IAudioEndpointVolume`, `sndDevicesInit.cpp:276-327`), sets
   `stopAudioCaptureAndPlaybackLoop = 1`, frees the three float buffers, and
   `UnregisterEndpointNotificationCallback` (`sndDevicesInit.cpp:247-267`).

`killProcessingThread` (`:263-322`) first calls `sndDevicesStartStopCapture(STOP)` so
`sndDevicesDoCapture` returns `CAPTURE_FORCED_EXIT`, then sets `i_kill_processing_thread_` and polls
`GetExitCodeThread` every 50 ms up to 3000 ms.

`sndDevicesRestoreDefaultDevice` (`sndDevicesSetupDevices.cpp:545-644`) picks the first *active*
device from: `user_selected_playback` → `most_recent_playback` → `most_recent_default` →
`prior_default` → `original_default` (`:617-630`) and `SetDefaultEndpoint`s it (`:635`). It is also
called on power-off from the GUI (`fxsound/Source/GUI/FxController.cpp:1747`).

---

## 17. Windows → Linux mapping table

| Windows mechanism | Purpose | Linux / PipeWire equivalent |
| --- | --- | --- |
| FxSound Audio Enhancer kernel driver (render endpoint) | The sink everything renders into | A **PipeWire sink node owned by the app process** (`media.class = Audio/Sink`). No kernel driver, no root, no reboot. |
| `IMMDeviceEnumerator::EnumAudioEndpoints(eRender, …)` | Enumerate outputs | `pw_registry` globals of type `PipeWire:Interface:Node` filtered on `media.class == "Audio/Sink"`. |
| `IMMDevice::GetId()` endpoint ID string | Stable identity | `node.name` (e.g. `alsa_output.pci-0000_00_1f.3.analog-stereo`). Persist this; use `object.serial`/`object.id` only as a runtime handle. |
| `PKEY_Device_FriendlyName` | UI label | `node.description`. |
| `PKEY_Device_DeviceDesc` | Short label | `node.nick`, falling back to `node.description`. |
| `PKEY_AudioEndpoint_FormFactor` | Icon/category | `device.form-factor` (`internal`/`speaker`/`headphone`/`headset`/`hdmi`/`webcam`…), `device.icon-name`, `device.bus` (`pci`/`usb`/`bluetooth`), `api.bluez5.*`. |
| `DEVICE_STATE_ACTIVE / UNPLUGGED / DISABLED / NOTPRESENT` | Availability | Node presence in the registry + `Node` info `state` (`suspended`/`idle`/`running`/`error`) + `device.profile`/`Route` availability (`available: yes/no/unknown`). A node that exists is "active"; an unavailable Route is "unplugged". |
| `IMMNotificationClient::OnDeviceAdded/Removed/StateChanged` | Hot-plug | Registry `global` / `global_remove` events. |
| `OnDefaultDeviceChanged` | Default changed | `Metadata` object named `default`, key `default.audio.sink` changed event. |
| `IPolicyConfigVista::SetDefaultEndpoint` (undocumented COM) | Set system default | `Metadata` `default`, subject `0`, key `default.configured.audio.sink`, value `{"name":"<node.name>"}` — exactly what `wpctl set-default` writes. **Documented and supported.** |
| `IPolicyConfig::SetEndpointVisibility` | Enable/disable endpoint | Not needed (our sink is a process, not a device). Closest analogue: destroy the node. |
| `IPolicyConfigVista::SetDeviceFormat` | Force the virtual device's rate/channels | We own the sink; just declare the format in `EnumFormat`/`pw_stream_connect`. |
| `AUDCLNT_STREAMFLAGS_LOOPBACK` | Capture what apps render | Not needed: our sink *receives* the audio directly in its `process()` callback. (If a monitor-based design were used instead, it would be `stream.capture.sink=true` + `target.object=<our sink>.monitor`.) |
| `AUDCLNT_SHAREMODE_SHARED` + timer polling + `Sleep(1)` | Pacing | PipeWire's graph clock drives `process()`; **never sleep or poll**. |
| `IAudioClient::GetCurrentPadding` | Ring fill level | `spa_io_buffers` / `pw_stream_dequeue_buffer` + our own ring-buffer fill counter; `pw_stream_get_time_n()` for `queued`/`delay`. |
| `IAudioRenderClient::GetBuffer/ReleaseBuffer` | Render | `pw_stream_dequeue_buffer()` → fill `buf->datas[0].data` → set `chunk->{offset,stride,size}` → `pw_stream_queue_buffer()`. |
| `AUDCLNT_BUFFERFLAGS_SILENT` | Silent packet | `chunk->size == 0`, or `SPA_CHUNK_FLAG_EMPTY` in `chunk->flags`. |
| `IAudioEndpointVolume` + `guidEventContext` | Volume/mute mirror + echo suppression | `SPA_PARAM_Props` (`volume`, `channelVolumes`, `mute`) on our own node. **Do not mirror to the real sink** — see §19.7. Echo suppression is unnecessary because we only write our own node. |
| `AUDCLNT_SESSIONFLAGS_DISPLAY_HIDE` | Hide the duplicate slider | `node.link-group` groups the sink and its output stream so UIs treat them as one unit; optionally `node.hidden = true` on the output stream. |
| `THREAD_PRIORITY_TIME_CRITICAL` | RT priority | PipeWire's data thread, made RT by `module-rt` via **RTKit** (`rt.prio`, typically 88). The app does nothing except keep `process()` RT-safe. |
| `CreateThread` + 100 ms supervisor timer + `INVALID_HANDLE_VALUE` sentinel | Restart policy | PipeWire keeps the graph alive; we only need a **reconnect** loop for `pw_core` errors (§22), with the same "don't hammer" backoff. |
| Registry `HKCU\SOFTWARE\DFX\13\23\devices\*` | Persistence | `$XDG_CONFIG_HOME/fxsound/audio.toml` (default `~/.config/fxsound/audio.toml`). |
| Global hotkeys (`RegisterHotKey`) | — | **Not available to a Wayland client.** Delegate to the compositor keybinding → D-Bus/CLI, and expose transport-ish controls over **MPRIS2** (`org.mpris.MediaPlayer2.fxsound`) if needed. Do not attempt an evdev grab. |
| WTS session check (`WTSGetActiveConsoleSessionId`) | Ignore other users' events | The PipeWire socket is per-user-session already; no equivalent needed. |

---

## 18. Linux design: comparison of the three options

### Option A — `pipewire-rs` native nodes in the app process ✅ **CHOSEN**

Create, from inside the FxSound process, (1) a sink node that apps render into and (2) a playback
stream that renders to the user-selected real sink, with our DSP in between.

| Pros | Cons |
| --- | --- |
| The DSP is our Rust code, in our process, with our parameter model — no LADSPA/LV2 wrapper, no plugin ABI, no separate `.so` to ship and locate. | Two nodes must be rate-matched if they end up on different graph drivers. |
| Zero external tooling: no `pw-cli`, `pactl`, `wpctl`, `pw-loopback` subprocesses, no shelling out, no parsing CLI output. | You must implement `process()` correctly and RT-safely yourself. |
| The sink disappears the instant the process exits (nodes are owned by the client connection) — **no orphaned null-sink left behind on a crash**. This is the single biggest operational win over Option B. | Slightly more code than `pw-cli create-node`. |
| Parameters (EQ, effects) change with zero graph churn — just write into an `ArcSwap`/triple-buffer read by `process()`. | |
| Full access to `SPA_IO_Position` (quantum, rate), `SPA_IO_RateMatch`, `Latency` params. | |
| One process to supervise; reconnect logic is a single `pw_core` listener. | |

**Crates:** `pipewire` (the official `pipewire-rs` bindings; use `0.8.x`) and `libspa` +
`libspa-sys` (re-exported by `pipewire` as `pipewire::spa`). Both are thin FFI over
`libpipewire-0.3.so`, which must be present at runtime (it is, on any PipeWire desktop).

### Option B — a null sink created via `pw-cli` / `pactl` / `wpctl`, captured from its monitor

```sh
pactl load-module module-null-sink sink_name=fxsound sink_properties=device.description=FxSound
# then capture from fxsound.monitor and play to the real sink
```

| Pros | Cons |
| --- | --- |
| Trivially few lines to get a sink. | **Orphan risk**: if the app crashes or is SIGKILLed, the null sink stays loaded and — if it was made the default — the user is left with a silent system. This is precisely the "affects system audio for all users" failure `CLAUDE.md` warns about. |
| Users can inspect it with familiar tools. | Requires `pactl`/`pipewire-pulse` (not guaranteed present) or `pw-cli create-node`, i.e. shelling out and parsing. |
| | Adds a **monitor hop**: sink → monitor → our capture stream, which is an extra buffer of latency and an extra copy. |
| | Module unload/reload races on PipeWire restart. |
| | No way to express our own `Latency`/`Props` on the sink. |

**Rejected.** If a fallback is ever needed (e.g. a distro shipping an ancient PipeWire), implement it
behind the same `AudioBackend` trait, and register an `atexit`/signal handler that unloads the module.

### Option C — `module-filter-chain` with a LADSPA/LV2 plugin

```
context.modules = [{ name = libpipewire-module-filter-chain
  args = { node.description = "FxSound"
           media.name = "FxSound"
           filter.graph = { nodes = [ { type = ladspa plugin = fxsound_dsp … } ] }
           capture.props  = { media.class = Audio/Sink  audio.channels = 2 }
           playback.props = { node.passive = true } } }]
```

| Pros | Cons |
| --- | --- |
| PipeWire does all the plumbing: one node that is a sink and whose output is linked to the default sink; rate matching, channel handling, latency reporting all handled. | The DSP has to be built as a **separate LADSPA/LV2 `.so`** with a C ABI, duplicating the parameter surface. |
| Config is declarative and inspectable. | Parameter updates from the GUI have to go through LADSPA control ports, reachable only via `pw-cli s <id> Props { params = [ … ] }` — clumsy, string-typed, and lossy for a 20-band EQ + 5 effects. |
| Survives app restarts (it's a daemon module). | Which is also a **con**: the filter keeps running with no GUI, and unloading it needs `pw-cli destroy`. |
| | Installing a module config means writing into `~/.config/pipewire/pipewire.conf.d/` — mutating the user's daemon config, and requiring a daemon restart or `pw-cli load-module`. |
| | Preset load = rebuilding the graph. Unacceptable for a live EQ. |

**Rejected** as the primary path. It remains the right answer for a *headless* FxSound service, and
the LADSPA wrapper is a reasonable **future** deliverable — but not the app's audio path.

---

## 19. Linux design: the chosen architecture in full

### 19.1 Topology

```
 ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
 │ Firefox      │  │ mpv          │  │ Spotify      │     ← Stream/Output/Audio nodes
 └──────┬───────┘  └──────┬───────┘  └──────┬───────┘
        └─────────────────┴─────────────────┘
                          │  links created by WirePlumber because
                          │  default.configured.audio.sink == "fxsound_sink"
                          ▼
 ╔═══════════════════════════════════════════════════════════════════════════╗
 ║  NODE 1  "fxsound_sink"          media.class = Audio/Sink                  ║
 ║  pw_stream, direction = Input,  node.link-group = "fxsound"                ║
 ║                                                                            ║
 ║   process():   dequeue → (deinterleave view) → DSP in place → push to ring ║
 ╚══════════════════════════════════╤════════════════════════════════════════╝
                                    │  SPSC ring buffer (lock-free, preallocated)
                                    │  target fill = 1.5 × quantum
 ╔══════════════════════════════════▼════════════════════════════════════════╗
 ║  NODE 2  "fxsound_output"        media.class = Stream/Output/Audio         ║
 ║  pw_stream, direction = Output, node.link-group = "fxsound"                ║
 ║  target.object = "<chosen real sink node.name>"                            ║
 ║                                                                            ║
 ║   process():   dequeue → pop from ring (or zero-fill on underrun) → queue  ║
 ╚══════════════════════════════════╤════════════════════════════════════════╝
                                    ▼
 ┌───────────────────────────────────────────────────────────────────────────┐
 │  alsa_output.pci-0000_00_1f.3.analog-stereo   (the real sink)              │
 └───────────────────────────────────────────────────────────────────────────┘
```

`node.link-group = "fxsound"` on **both** nodes is mandatory: it is how `module-loopback` and
`module-filter-chain` tell WirePlumber that these two nodes are one logical unit, which (a) makes the
policy refuse to link `fxsound_output` back into `fxsound_sink` (a feedback loop that would otherwise
happen the moment our sink becomes the default), and (b) makes UIs present them as one device.

### 19.2 DSP placement

Do the DSP in **NODE 1's** `process()`, not NODE 2's. Reasons:
* NODE 1 sees exactly the format we declared (we control it); NODE 2's format is whatever the target
  sink negotiated.
* It matches the Windows semantics (`processAudio` runs on the captured buffer before any rate
  conversion).
* If NODE 2 underruns, we emit silence rather than re-running the DSP on stale data.

### 19.3 Format policy (replaces §8 entirely)

| Windows | Linux |
| --- | --- |
| Push the DFX device to 44100 or 48000 based on `playbackRate % 48000` | **Run the virtual sink at the target sink's own `audio.rate`** (read from the sink node's `Format`/`node.rate`, or from `clock.rate` in `SPA_IO_Position`). Fall back to `48000`. |
| Integer zero-order-hold upsample | **Never resample ourselves.** If the target sink runs at a different rate, PipeWire's `adapter` resamples with its sinc resampler at whatever quality `resample.quality` is set to (default 4). |
| Clamp channels to `[2,8]`, quad → 6, mono → silence | Keep the clamp: `channels = clamp(target_channels, 2, 8)`. **Refuse mono targets** exactly as Windows does (`SND_DEVICES_NO_VALID_PLAYBACK_DEVICE`), and surface the same two user-facing states. |
| Hand-written 6→2/6→4/6→8/2→4 mixdowns | Declare `audio.position` on NODE 1 to match the target's `audio.position` and let PipeWire's channel mixer handle client remixing into us. Only the *identity* case then exists inside `process()`. If parity with FxSound's deliberate "fill side channels with back channels" 6→8 upmix is wanted, set `stream.dont-remix = true` on NODE 2 and reproduce the table in §11 — but default to letting PipeWire do it. |
| Format is always F32 interleaved | Same: `SPA_AUDIO_FORMAT_F32` (native-endian `F32`, i.e. `F32LE` on x86/ARM LE). Ask for `SPA_AUDIO_FORMAT_F32P` (planar/DSP) on NODE 1 if the DSP prefers deinterleaved — PipeWire supports both; interleaved `F32` is the simpler port since `DfxDsp::processAudio` takes interleaved. |

`SND_DEVICES_MAX_SAMP_FREQ = 192000` (`sndDevices.h:189`) remains the upper clamp;
`SND_DEVICES_MIN_NUM_CHANS = 2` / `SND_DEVICES_MAX_NUM_CHANS = 8` (`:190-191`) remain the channel
clamps.

### 19.4 Device model

```rust
/// The Linux analogue of `SoundDevice` (AudioPassthru.h:32-53).
#[derive(Clone, Debug, PartialEq)]
pub struct SoundDevice {
    /// `node.name` — stable across reboots. THE identity. (≡ pwszID)
    pub id: String,
    /// runtime handle only; changes on every PipeWire restart
    pub object_id: u32,
    pub object_serial: u64,
    /// `node.description` (≡ deviceFriendlyName)
    pub friendly_name: String,
    /// `node.nick` (≡ deviceDescription)
    pub description: String,
    /// mapped from `device.form-factor` / `device.icon-name` / `device.bus`
    pub form_factor: FormFactor,
    pub channels: u32,
    pub rate: u32,
    /// node exists and its Route is available
    pub is_active: bool,
    /// `default.audio.sink` == self.id
    pub is_default: bool,
    /// self.id == our own virtual sink
    pub is_virtual_sink: bool,
    /// currently the render target of NODE 2
    pub is_targeted_output: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormFactor {
    Speakers, Headphones, Headset, LineLevel, Spdif, Hdmi,
    DigitalPassthrough, NetworkDevice, Handset, Microphone, Unknown,
}
```

`FormFactor` mapping (keeps the same 11 variants as `sndDevices_GetAll.cpp:198-208` so the GUI's
icon table ports 1:1):

| PipeWire property → value | `FormFactor` |
| --- | --- |
| `device.form-factor = "internal"` / `"speaker"` | `Speakers` |
| `device.form-factor = "headphone"` | `Headphones` |
| `device.form-factor = "headset"` | `Headset` |
| `device.form-factor = "hands-free"` / `"handset"` | `Handset` |
| `device.form-factor = "microphone"` | `Microphone` |
| `device.form-factor = "tv"` or `node.name` contains `.hdmi-` or `device.profile.name` starts `hdmi-` | `Hdmi` |
| `node.name` contains `.iec958-` / `device.profile.name` contains `iec958` | `Spdif` |
| `device.bus = "bluetooth"` / `api.bluez5.*` present | `Headphones` (or `Headset` if `api.bluez5.profile` is `headset-head-unit`) |
| `media.class = Audio/Sink` on a `Network`/`RAOP`/`roc` module | `NetworkDevice` |
| anything else | `Unknown` |

### 19.5 Output-selection rules (Linux port of §7)

Persisted in `~/.config/fxsound/audio.toml`:

```toml
schema          = 1
original_default      = "alsa_output.pci-0000_00_1f.3.analog-stereo"
most_recent_default   = "alsa_output.pci-0000_00_1f.3.analog-stereo"
prior_default         = ""
most_recent_playback  = "alsa_output.usb-Focusrite_Scarlett_2i2-00.analog-stereo"
user_selected_playback = ""
auto_default_mode     = true      # ≡ default_device_mode "on"/"off"
buffer_ms             = 40        # ≡ user_buffer_size
```

Algorithm — identical shape to `sndDevicesImplementDeviceRules`, with two substitutions:
"real device" = any `Audio/Sink` node that is not ours, and "current default" =
`default.audio.sink` from the `default` metadata.

```
1. real_sinks.is_empty()                      -> Idle("no output devices")
2. most_recent_default == ""                  -> first run:
     if current_default != our_sink:
         original_default = most_recent_default = current_default
         target = current_default;  goto MonoCheck
3. real_sinks.len() == 1                       -> target = real_sinks[0];  goto MonoCheck
4. user_selected_playback resolves & active    -> target = it;  goto MonoCheck
5. a NEW sink appeared since last enumeration  -> target = first new sink with >= 2 channels
                                                  write_prev_default = true;  goto MonoCheck
6. current_default != our_sink                 -> target = current_default
                                                  write_prev_default = true;  goto MonoCheck
7. else (we are already the default)           -> first active of:
       most_recent_playback, most_recent_default, prior_default, original_default,
       else real_sinks[0]

MonoCheck:
   if target.channels == 1:
       retry most_recent_playback
       if that is also mono:
           non_mono = real_sinks.iter().filter(|d| d.channels >= 2).count()
           if non_mono >= 1 -> Error::AskUserSelectOutput      (≡ -58)
           else             -> Error::NoValidOutput            (≡ -57)

Commit:
   most_recent_playback = target.id
   if write_prev_default { most_recent_default = target.id; prior_default = <old most_recent_playback> }
   connect NODE 2 with target.object = target.id
```

### 19.6 The `process()` callbacks

**NODE 1 (sink):**

```rust
fn on_sink_process(&mut self) {
    let Some(mut buf) = self.sink_stream.dequeue_buffer() else { return };
    let datas = buf.datas_mut();
    let Some(d) = datas.first_mut() else { return };
    let chunk_size   = d.chunk().size() as usize;
    let stride       = d.chunk().stride().max(1) as usize;
    let n_frames     = chunk_size / stride;           // stride = channels * 4
    let Some(slice)  = d.data() else { return };

    // SILENT-packet equivalent of AUDCLNT_BUFFERFLAGS_SILENT
    // (sndDevicesDoCapture.cpp:186-191): chunk.size == 0 => treat as zeros.
    let samples: &mut [f32] = bytemuck::cast_slice_mut(&mut slice[..chunk_size]);

    if !self.bypass.load(Relaxed) {
        // Format changes are latched by the param_changed callback, never here.
        self.dsp.process_in_place(samples, n_frames);   // ≡ DfxDsp::processAudio
    }
    if self.muted.load(Relaxed) {
        samples.fill(0.0);                               // ≡ AudioPassthru::mute(true)
    }
    // Never blocks; on overrun it DROPS the oldest quantum and bumps a counter.
    self.ring.push_frames(samples, n_frames);
}
```

**NODE 2 (output):**

```rust
fn on_out_process(&mut self) {
    let Some(mut buf) = self.out_stream.dequeue_buffer() else { return };
    let want = self.quantum();                       // position.clock.duration
    let d    = &mut buf.datas_mut()[0];
    let stride = (self.channels * 4) as u32;
    let cap    = d.as_raw().maxsize / stride;
    let n      = want.min(cap as u64) as usize;
    let out: &mut [f32] = bytemuck::cast_slice_mut(&mut d.data().unwrap()[..n * self.channels * 4]);

    let got = self.ring.pop_frames(out, n);
    if got < n { out[got * self.channels..].fill(0.0); self.underruns += 1; }

    let chunk = d.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut()   = (n as u32) * stride;
    self.out_stream.queue_buffer(buf).ok();
}
```

The ring's **target fill** is `1.5 × quantum` frames; the startup rule mirrors the Windows
"don't start until half the capture buffer is full" (`sndDevicesDoCapture.cpp:129-140`):
NODE 2 outputs silence until the ring first reaches `target_fill`, then runs.

### 19.7 Volume and mute — the opinionated departure

**Do not port the volume mirror.** On Windows, the DFX endpoint and the real endpoint are two
separate system volume controls, so FxSound mirrors them (§13) to give the user one apparent slider.
On Linux, once `fxsound_sink` is the default sink it **is** the control that `XF86AudioRaiseVolume`,
`wpctl set-volume @DEFAULT_AUDIO_SINK@`, GNOME's slider and pavucontrol all target. Mirroring would
be actively harmful: it would fight WirePlumber's own volume restore, and it would leave the real
sink's volume permanently changed after we exit.

Concretely:

* FxSound's volume/mute live on **our own node's `SPA_PARAM_Props`** (`volume`, `channelVolumes`,
  `mute`). Read them in `param_changed` and apply them in `process()` (or let PipeWire's adapter
  apply them — preferred, since it is already doing volume ramping).
* **Never write `Props` on the target sink.** Therefore no save/restore of the real sink's volume is
  needed, and the whole `savedPlaybackVolume` / `savedPlaybackMute` / `playbackSettingsAreSaved` /
  `b_never_raise_volume` machinery (`sndDevicesSetupDevices.cpp:232-278`) **disappears**.
* If a future release *does* want to drive the hardware volume (e.g. to use a DAC's analogue gain),
  it must reintroduce the same contract: save before the first write, restore on switch-away without
  clamping, restore on hand-back **clamped to never raise**.
* `mute(true)` writes zeros into the output rather than stopping NODE 2, so the graph and the
  device stay warm and no xrun storm follows an unmute.

### 19.8 Idle/suspend behaviour

Windows stops the render client when the ring empties, "to allow PC to sleep"
(`sndDevicesDoCapture.cpp:134-139`). PipeWire does this for us: a sink with no active links goes
`idle` and then `suspended` per `suspend-node` / `session.suspend-timeout-seconds` (WirePlumber
default 5 s). Do **not** fight it. Set `node.always-process = false` (the default) so our sink can
suspend; set it to `true` only if you observe first-sound truncation on a specific device.

---

## 20. Exact node properties

### NODE 1 — the virtual sink

| Key | Value | Why |
| --- | --- | --- |
| `media.class` | `"Audio/Sink"` | Makes it a sink that apps and WirePlumber will target. |
| `node.name` | `"fxsound_sink"` | The stable ID we write into the default metadata. |
| `node.description` | `"FxSound"` | What the user sees in the sound settings list. |
| `node.nick` | `"FxSound"` | |
| `node.virtual` | `"true"` | Tells UIs/policy this is not hardware. |
| `node.link-group` | `"fxsound"` | **Mandatory** — prevents the feedback loop with NODE 2. |
| `device.class` | `"sound"` | Some UIs group by this. |
| `media.icon-name` / `application.icon-name` | `"fxsound"` | Icon lookup. |
| `audio.rate` | target sink's rate, else `48000` | |
| `audio.channels` | `clamp(target.channels, 2, 8)` | Matches `sndDevices.h:190-191`. |
| `audio.position` | e.g. `"FL,FR"`, `"FL,FR,FC,LFE,RL,RR"` | Mirror the target's positions. |
| `audio.format` | `"F32"` | |
| `node.want-driver` | `"true"` | Lets the graph driver (the real device) drive us instead of us becoming a driver. |
| `node.always-process` | `"false"` | Allow suspend when idle. |
| `node.latency` | `"<quantum>/<rate>"`, e.g. `"1024/48000"` | Requests our quantum. |
| `priority.session` | `1010` | Slightly above typical ALSA sinks (≈1000) so that, if the user has never chosen a default, policy prefers us. Set to `500` if you would rather never win by default. |
| `priority.driver` | `0` | We are not a driver. |
| `monitor.channel-volumes` | `"false"` | |

### NODE 2 — the output stream

| Key | Value | Why |
| --- | --- | --- |
| `media.class` | `"Stream/Output/Audio"` | An ordinary playback stream. |
| `media.category` | `"Playback"` | |
| `media.role` | `"Production"` | Avoids being ducked by role policies (do **not** use `"Music"` — it can be corked by phone-call roles). |
| `node.name` | `"fxsound_output"` | |
| `node.description` | `"FxSound output"` | |
| `node.link-group` | `"fxsound"` | **Mandatory**, same group as NODE 1. |
| `node.autoconnect` | `"true"` | |
| `target.object` | the chosen sink's `node.name` (or `object.serial` as a string) | The modern replacement for the deprecated `node.target`. |
| `node.dont-reconnect` | `"false"` | We *want* it to move if the target vanishes; we will then re-run the rules. |
| `node.passive` | `"false"` | Keep the device awake while audio flows. |
| `stream.dont-remix` | `"false"` (default) — set `"true"` only if reproducing §11's hand-written upmixes | |
| `node.latency` | same `"<quantum>/<rate>"` as NODE 1 | |
| `node.hidden` | `"true"` *(optional)* | Hides the duplicate entry in pavucontrol; the analogue of `AUDCLNT_SESSIONFLAGS_DISPLAY_HIDE` (`sndDevicesSetupDevices.cpp:172`). **Verify per-UI before shipping** — see Open Questions. |

### Format declaration (`libspa` POD)

```rust
use libspa::param::audio::{AudioFormat, AudioInfoRaw};
let mut info = AudioInfoRaw::new();
info.set_format(AudioFormat::F32LE);   // native-endian F32 on LE targets
info.set_rate(rate);                   // e.g. 48000
info.set_channels(channels);           // 2..=8
info.set_position(positions);          // [SPA_AUDIO_CHANNEL_FL, FR, ...]
let obj = libspa::pod::Object {
    type_: libspa::utils::SpaTypes::ObjectParamFormat.as_raw(),
    id:    libspa::param::ParamType::EnumFormat.as_raw(),
    properties: info.into(),
};
```

Connect flags for both streams:
`StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS`.
`RT_PROCESS` is what puts `process()` on the data thread — which is exactly why it must be RT-safe
(§23).

---

## 21. Becoming the default sink, politely

The PipeWire analogue of `IPolicyConfigVista::SetDefaultEndpoint`
(`sndDevicesSet.cpp:112`) is the **`default` metadata object**.

```
Metadata object:  props["metadata.name"] == "default"
subject:          0   (= "the session")
keys:             default.audio.sink              (what is in effect NOW, written by WirePlumber)
                  default.configured.audio.sink   (the USER'S CHOICE — write this one)
value type:       "Spa:String:JSON"
value:            {"name":"fxsound_sink"}
```

### The polite protocol

1. **Read and persist first.** Before writing anything, read `default.configured.audio.sink`. If it
   is absent, read `default.audio.sink`. Store the `name` in `original_default` (first run only) and
   in `most_recent_default`, exactly as `sndDevicesImplementDeviceRules.cpp:156-159` does.
2. **Ask, do not assume.** Only take the default when the user has FxSound powered on. The Windows
   code does this unconditionally because the driver exists solely for FxSound; our sink is a
   process, and hijacking the default at install time would be rude. Gate it behind the same
   `auto_default_mode` flag (`default_device_mode`, `sndDevices.h:165`), default `true`, and expose
   it in Settings.
3. **Write.**
   ```rust
   metadata.set_property(0, "default.configured.audio.sink",
                         Some("Spa:String:JSON"),
                         Some(r#"{"name":"fxsound_sink"}"#));
   ```
   Equivalent CLI, for docs and for the fallback path: `wpctl set-default <id>`, or
   `pw-metadata -n default 0 default.configured.audio.sink '{"name":"fxsound_sink"}'`.
4. **Existing streams.** WirePlumber's default policy moves already-running streams to the new
   default (`node.stream.default-playback` "follow"). Streams pinned with `node.dont-reconnect` or an
   explicit `target.object` stay put — that is correct and must not be overridden.
5. **Restore on exit**, in this order (this is the Linux `sndDevicesRestoreDefaultDevice`,
   `sndDevicesSetupDevices.cpp:545-644`):
   1. Pick the first *present* sink from `user_selected_playback` → `most_recent_playback` →
      `most_recent_default` → `prior_default` → `original_default`.
   2. Write it into `default.configured.audio.sink`.
   3. **Only then** disconnect the streams / destroy the nodes.
   Doing it in that order means there is never a window in which the default points at a node that
   no longer exists. Do this on `SIGINT`/`SIGTERM` as well as on clean quit — install a handler that
   performs step 5 and then re-raises.
6. **Never** write `default.audio.sink` directly; that is WirePlumber's to own.
7. If no metadata object is present (a bare `pipewire` with no session manager), skip silently and
   log — the user can still link manually with `helvum`/`qpwgraph`.

---

## 22. Surviving a PipeWire restart

PipeWire is `Restart=on-failure` under systemd `--user`; `systemctl --user restart pipewire` is a
routine troubleshooting step. When the daemon goes away:

* `pw_core`'s `error` listener fires with `id == PW_ID_CORE` and a negative `res`
  (typically `-EPIPE`/`-ECONNRESET`), **or** the socket simply closes.
* Every proxy (registry, metadata, streams) is dead. Do not touch them afterwards.

**Required behaviour** — this is the direct analogue of `processTimer`'s restart state machine
(`AudioPassthruPrivate.cpp:359-474`), including the "don't hammer" rule:

```
State machine (runs on the pw_thread_loop):

  Disconnected ──connect ok──► Connecting ──core.done(seq)──► Registry sync
        ▲                                                          │
        │                                                          ▼
        │                                                   Enumerating sinks
        │                                                          │
        │                                                     rules pick target
        │                                                          ▼
        │                                              Running (both streams connected)
        │                                                          │
        └───────────────── core.error / socket EOF ────────────────┘

  Backoff: 200 ms, 400 ms, 800 ms, 1600 ms, 3200 ms, then 5000 ms flat.
  Never retry faster than 200 ms  (the Windows equivalent of the INVALID_HANDLE_VALUE
  pause at AudioPassthruPrivate.cpp:452-460, which exists to avoid burning CPU and
  leaking objects on a retry loop).
```

Implementation notes:

* Keep the `pw_context` and `pw_thread_loop` alive across reconnects; only `pw_core` and everything
  below it is recreated. `pw_context_connect` is cheap.
* Tear down in reverse order: streams → metadata proxy → registry → core.
* On reconnect, **re-assert the default sink** if `auto_default_mode` is on and we had it before —
  WirePlumber will have restored the user's configured default, which is now us (because we wrote
  `default.configured.audio.sink`, which is persisted by WirePlumber's state files). Verify rather
  than blindly rewrite.
* The GUI must be told: emit the equivalent of `onSoundDeviceChange(processing = true/false)`
  (`AudioPassthru.h:58`) on every state transition so the UI can show "reconnecting…".
* Also handle the softer case: **the target sink disappears** (USB DAC unplugged). That is a
  registry `global_remove` for that node id → re-run the §19.5 rules → reconnect NODE 2 with a new
  `target.object`. Do not tear down NODE 1; apps stay connected to it and never notice.
* Watch for `PIPEWIRE_REMOTE` / `XDG_RUNTIME_DIR` being unset (e.g. under a bare TTY or a flatpak
  without the `pipewire` socket permission) and fail with a clear message rather than looping.

---

## 23. Real-time thread constraints

`process()` runs on PipeWire's **data thread**, which `module-rt` has already promoted to
`SCHED_FIFO` via **RTKit** (`org.freedesktop.RealtimeKit1`) — typically `rt.prio = 88`,
`rt.time.soft = 200000` µs, `rt.time.hard = 200000` µs, `nice.level = -11` in the shipped
`pipewire.conf`. **The application does not need to request RT priority itself**; this replaces the
`SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)` at
`AudioPassthruPrivate.cpp:493`.

Inside `process()` the following are **forbidden** (this is the `CLAUDE.md` "no allocations, locks or
blocking calls in the audio callback path" rule, made concrete):

| Forbidden | Use instead |
| --- | --- |
| `Box::new`, `Vec::push`/`resize`, `String`, `format!`, `collect()` | Preallocate in the reconfigure path; `process()` only writes into existing slices. |
| `Mutex`, `RwLock`, `RefCell` borrow panics, channel `send` that can block | `arc_swap::ArcSwap<DspParams>` for parameter snapshots; `triple_buffer` for larger state; `AtomicU32`/`AtomicBool` for flags (`Ordering::Relaxed` is enough for scalars). |
| `println!`, `log::info!`, any syscall, `std::fs` | Push a `u32` code into a lock-free SPSC queue drained by the GUI thread. |
| `panic!` / `unwrap()` / slice indexing that can panic | Every `dequeue_buffer`/`data()` returns `Option`; use `let … else { return }`. A panic on the data thread aborts the process and kills system audio. |
| Denormals | Set FTZ/DAZ once on the data thread (`_MM_SET_FLUSH_ZERO_MODE` / `_MM_SET_DENORMALS_ZERO_MODE`, or add a tiny DC offset) — a denormal storm in an IIR EQ is the classic cause of xruns. |
| Blocking on the other node's callback | The SPSC ring must be wait-free on both ends; on overrun drop, on underrun zero-fill. Count both. |

The **quantum** (frames per `process()` call) is *not* constant and *must* be read per call from
`SPA_IO_Position`:

```rust
let pos  = unsafe { &*self.stream.get_io::<libspa::sys::spa_io_position>() };
let n    = pos.clock.duration as usize;        // frames this cycle
let rate = pos.clock.rate.denom;               // e.g. 48000
```

If the two nodes ever land on different drivers, `SPA_IO_RateMatch` (`rate_match.rate`) gives the
drift correction factor; the simpler and recommended answer is to keep
`node.want-driver = "true"` on NODE 1 so the real device drives both, in which case the ring's fill
level is constant and no rate matching is needed.

Buffer memory is shared via memfd and mapped by `StreamFlags::MAP_BUFFERS`; do not assume
`datas[0].data` is the same pointer across calls, and always honour `maxsize` and `stride`.

---

## 24. Latency budget and recommended buffer sizes

### The Windows numbers, for reference

| Quantity | Value | Source |
| --- | --- | --- |
| Compiled-in default "average delay" | **80 ms** | `sndDevices.h:199` → `:194` |
| Actual WASAPI ring per side | **160 ms** (`ms × 1e7 / 500`) | `sndDevicesReInit.cpp:261-262` |
| Recommended on 64-bit Windows (unused) | 40 ms | `sndDevices.h:198`, `sndDevicesGet.cpp:748` |
| User range | 10 … 100 ms | `sndDevices.h:200-201`; clamped `sndDevicesSet.cpp:541-545` |
| Steady-state target fill | half the capture ring = **40 ms** at the 80 ms setting | `sndDevicesDoCapture.cpp:125`, `:152-156` |
| Drain threshold | quarter the capture ring = **20 ms** | `:126`, `:262` |
| Polling granularity | `Sleep(1)` | `:99` |
| Effective end-to-end | ≈ **80–120 ms** | derived |

### The Linux recommendation

Run **much** tighter — PipeWire's event-driven graph removes the `Sleep(1)` polling tax entirely.

| Setting | Quantum @48 kHz | Per-node latency | Ring target (1.5×) | **Total end-to-end** | Use when |
| --- | --- | --- | --- | --- | --- |
| `Low` | **256** | 5.33 ms | 8.0 ms | ≈ **24 ms** | Fast CPU, no Bluetooth, user wants lip-sync |
| **`Normal` (DEFAULT)** | **512** | 10.67 ms | 16.0 ms | ≈ **43 ms** | Everything, ship this |
| `Safe` | **1024** | 21.33 ms | 32.0 ms | ≈ **85 ms** | Bluetooth, heavy 31-band EQ, weak CPU |
| `Max` | **2048** | 42.67 ms | 64.0 ms | ≈ **170 ms** | Last-resort anti-crackle |

Total = NODE 1 quantum + ring target + NODE 2 quantum + device buffer (≈ 2 quanta on ALSA).

**Concrete allocation sizes** (allocate once, at reconfigure, never in `process()`):

```
MAX_QUANTUM      = 2048 frames
MAX_CHANNELS     = 8                      // SND_DEVICES_MAX_NUM_CHANS (sndDevices.h:191)
RING_CAPACITY    = 8 * MAX_QUANTUM        = 16 384 frames
RING_BYTES       = 16 384 * 8 * 4         = 524 288 bytes  (512 KiB)
SCRATCH (per node) = MAX_QUANTUM * MAX_CHANNELS * 4 = 65 536 bytes
```

Mapping the legacy `buffer_ms` setting (10…100, `sndDevices.h:200-201`) onto a quantum:

```rust
fn quantum_for_ms(ms: u32, rate: u32) -> u32 {
    let ms = ms.clamp(10, 100);                    // ≡ sndDevicesSetBufferSizeMilliSecs:541-545
    let frames = (ms as u64 * rate as u64 / 1000) as u32;
    // round DOWN to a power of two in [256, 2048]; PipeWire tolerates non-powers
    // of two but graph-wide quantum negotiation behaves best with them.
    frames.next_power_of_two().max(256).min(2048) >> 1
}
```

Request it with `node.latency = format!("{quantum}/{rate}")` on both nodes. Note that PipeWire takes
the **minimum** requested latency across all nodes in the graph as the graph quantum, clamped to
`default.clock.min-quantum` … `default.clock.max-quantum` — so we can only ever *lower* the graph
quantum, never raise it. Handle a graph quantum larger than we asked for gracefully (the ring is
sized for 2048 either way).

**Xrun policy:** count underruns and overruns as atomics; if underruns exceed 10 in 5 seconds,
automatically step to the next larger setting once and tell the user. Never step down automatically.

---

## 25. Rust module layout, crates and types

```
fxsound-linux/
  crates/
    fx-audio/                       # everything in this document
      Cargo.toml
      src/
        lib.rs                      # pub use backend::*, device::*, error::*
        backend.rs                  # trait AudioBackend  (so the null-sink fallback can slot in)
        pipewire/
          mod.rs                    # PipewireBackend: context, thread_loop, reconnect FSM (§22)
          nodes.rs                  # NODE 1 / NODE 2 construction, all props from §20
          process.rs                # the two RT callbacks (§19.6, §23)
          registry.rs               # sink enumeration -> SoundDevice (§19.4)
          metadata.rs               # default-sink read/write/restore (§21)
          format.rs                 # AudioInfoRaw <-> our Format type (§19.3)
        ring.rs                     # wait-free SPSC f32 ring, preallocated
        rules.rs                    # §19.5 output selection, pure & unit-testable
        settings.rs                 # audio.toml (§19.5)
        device.rs                   # SoundDevice, FormFactor
        error.rs                    # AudioError (mirrors the -2/-54/-57/-58 set)
```

**Cargo dependencies (exact):**

```toml
[dependencies]
pipewire   = "0.8"          # pipewire-rs; re-exports `spa` (libspa)
libspa     = "0.8"          # explicit, for POD building
libspa-sys = "0.8"          # raw spa_io_position etc.
arc-swap   = "1"            # RT-safe parameter snapshots
bytemuck   = "1"            # &[u8] <-> &[f32] without unsafe at the call site
crossbeam-queue = "0.3"     # ArrayQueue for the RT->GUI event channel
serde      = { version = "1", features = ["derive"] }
toml       = "0.8"
thiserror  = "2"
```

Build requirement: `libpipewire-0.3` development headers at build time (`pkg-config
libpipewire-0.3 >= 0.3.60`), `libpipewire-0.3.so.0` at runtime. Document this in the README and fail
`build.rs` with a readable message.

**Error type** (keeps a 1:1 correspondence with the states the GUI already knows how to render):

```rust
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no output devices present")]            NoOutputDevices,        // ≡ 209
    #[error("selected output is not present")]        DeviceNotPresent,       // ≡ -2
    #[error("output device is unavailable")]          DeviceUnavailable,      // ≡ -54 + playbackDeviceIsUnavailable
    #[error("no usable (stereo or better) output")]   NoValidOutput,          // ≡ -57
    #[error("please choose an output device")]        AskUserSelectOutput,    // ≡ -58
    #[error("PipeWire is not available: {0}")]        PipewireUnavailable(String),
    #[error("lost connection to PipeWire")]           PipewireDisconnected,
    #[error("format negotiation failed")]             FormatNegotiation,      // ≡ -35/-36
}
```

**Public API** — deliberately the same shape as `AudioPassthru` so the GUI port is mechanical:

```rust
pub trait AudioBackend: Send {
    fn start(&mut self) -> Result<(), AudioError>;          // ≡ init()
    fn stop(&mut self);                                     // ≡ ~AudioPassthru
    fn devices(&self, active_only: bool) -> Vec<SoundDevice>;// ≡ getSoundDevices()
    fn set_output(&mut self, id: &str) -> Result<(), AudioError>; // ≡ setAsPlaybackDevice()
    fn set_muted(&self, muted: bool);                       // ≡ mute()
    fn set_buffer_ms(&mut self, ms: u32) -> Result<(), AudioError>; // ≡ setBufferLength()
    fn is_output_available(&self) -> bool;                  // ≡ isPlaybackDeviceAvailable()
    fn take_as_default(&mut self, yes: bool) -> Result<(), AudioError>; // §21
    fn restore_default(&mut self);                          // ≡ restoreDefaultPlaybackDevice()
    fn subscribe(&mut self, cb: Box<dyn Fn(AudioEvent) + Send + 'static>); // ≡ registerCallback()
}

pub enum AudioEvent {
    DevicesChanged,                  // ≡ onSoundDeviceChange(false)
    ProcessingRestarted,             // ≡ onSoundDeviceChange(true)
    OutputChanged(String),
    Error(AudioError),
    Xrun { underruns: u64, overruns: u64 },
}
```

**There is no `processTimer()`.** The 100 ms supervisor poll
(`fxsound/Source/GUI/FxController.cpp:1735`, `:2061`) exists only because Windows had no event
source for "the processing thread died". PipeWire gives us events for everything. The egui app
should still repaint on `AudioEvent` via `egui::Context::request_repaint()` rather than polling —
and if it *does* want a cheap heartbeat for the "audio is flowing" logo animation (the
`audio_process_on_counter_ == 5` logic at `FxController.cpp:2076`), derive it from a frame counter
incremented in `process()` and read atomically, not from a timer that calls into the backend.

---

## 26. Test plan

Because this module can silence a user's machine, the following must all be green before shipping:

1. **Orphan test.** `kill -9` the app while it is the default sink. Expect: sink node gone within
   one second (the socket closes), WirePlumber falls back to the next-highest-priority sink, audio
   returns. This is the test Option B cannot pass.
2. **Restore test.** Clean quit restores `default.configured.audio.sink` to the pre-launch value.
   Assert with `pw-metadata -n default`.
3. **SIGTERM test.** Same as (2) but via `systemctl --user stop` / `SIGTERM`.
4. **PipeWire restart test.** `systemctl --user restart pipewire pipewire-pulse wireplumber` while
   audio plays. Expect reconnect within 2 s, no more than one audible gap, backoff never tighter
   than 200 ms (assert with a counter).
5. **Hot-unplug test.** Unplug a USB DAC that is the current target mid-playback. Expect NODE 2 to
   move to the next device per §19.5, NODE 1 untouched, clients never disconnected.
6. **Mono test.** `pactl load-module module-null-sink channels=1` as the only output. Expect
   `NoValidOutput`; with a stereo sink also present, expect `AskUserSelectOutput`.
7. **Rate-change test.** Switch the target sink between 44.1/48/96 kHz. Expect no crash, no
   zero-order-hold artefacts (spectrum-analyse a 10 kHz sine for images).
8. **Xrun test.** Run at `Low` under `stress-ng --cpu $(nproc)`. Expect underruns counted and the
   auto-step-up to fire once.
9. **RT-safety test.** Build with a `process()` allocator shim that aborts on any allocation on the
   data thread; run for 10 minutes.
10. **Loop test.** Assert that WirePlumber never links `fxsound_output` into `fxsound_sink` (remove
    `node.link-group` once, observe the loop, put it back — so the regression is understood).

---

## Open questions / risks for the Rust port

1. **`node.hidden` on NODE 2.** It is the natural analogue of `AUDCLNT_SESSIONFLAGS_DISPLAY_HIDE`
   (`sndDevicesSetupDevices.cpp:172`), but behaviour varies: some WirePlumber versions skip policy
   linking for hidden nodes entirely, which would break us. **Ship with `node.hidden` unset**, rely
   on `node.link-group` for grouping, and only revisit after testing against WirePlumber ≥ 0.5 with
   pavucontrol, `wpctl status`, GNOME Settings and KDE Plasma's applet.

2. **`priority.session = 1010`.** Choosing a value above typical hardware sinks (≈1000) means that on
   a machine where the user has *never* picked a default, policy may auto-select FxSound the first
   time it runs. Is that desirable or presumptuous? The Windows product does exactly this, but only
   because the driver exists solely for FxSound. **Recommend shipping `500` (never wins implicitly)
   and taking the default only through the explicit §21 path** — but this is a product decision, not
   a technical one.

3. **Two nodes, one driver — is rate matching ever needed?** With `node.want-driver = "true"` on
   NODE 1 and NODE 2 linked to a real device, both should be scheduled by the device's driver in the
   same cycle, making the ring's fill level constant. If a configuration is found where they land on
   different drivers (e.g. a network sink, or `node.force-quantum` set by the user), the ring will
   drift and needs `SPA_IO_RateMatch`-driven adaptive resampling. **Instrument the ring fill level
   from day one** (log min/max over 60 s windows) so drift is detected in the field rather than
   guessed at.

4. **Dropping the volume mirror is a visible behaviour change.** Windows users are used to the
   FxSound slider and the device slider moving together (§13). Linux users will see exactly one
   slider (ours, because we are the default). This is correct and better, but it means the GUI's
   volume-related code paths need review, and the Windows-era `savedPlaybackVolume = 0.25f` default
   (`sndDevicesInit.cpp:111`) has no meaning here. Confirm with the product owner.

5. **Channel upmix parity.** §11's 6→8 mapping deliberately duplicates BL/BR into SL/SR, and 6→4
   drops FC/LFE. PipeWire's channel mixer will do something different (and more correct). If bit-for-bit
   parity with Windows surround output is a requirement, `stream.dont-remix = true` plus a hand-ported
   mixer is needed. **Assume it is not required** unless told otherwise.

6. **Mono output is refused, not downmixed.** Ported faithfully from
   `SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES` / `_FORCE_SILENCE` (`sndDevices.h:37-39`,
   `sndDevicesDoCapture.cpp:350-372`). On Linux there is no driver bug forcing this — a mono BT
   headset (HSP/HFP) is perfectly drivable. Consider **fixing** it (downmix to mono and allow the
   device) rather than porting the refusal, which would remove the `-57`/`-58` states entirely.
   Needs a decision; the spec above ports the refusal to stay faithful.

7. **The `isUserSelectedPlaybackDevice` bug.** `AudioPassthruPrivate.cpp:215` compares against an
   uninitialised buffer (§15). Nothing in the GUI reads the flag today, so the Linux port should
   simply **not have that field** — but verify no downstream (fxmcp, fxdiag) consumer depends on it.

8. **`user_selected_playback` is never written.** The registry value exists
   (`sndDevices.h:164`) and is read in four places, but no code path writes it; user selection is
   expressed by forcing the device to be the system default (`AudioPassthruPrivate.cpp:657`). The
   Linux rules in §19.5 keep the slot for fidelity. **Either wire it up properly** (write it in
   `set_output`, which is more honest and avoids mutating the global default just to record a
   preference) **or delete it.** Recommend wiring it up.

9. **Flatpak / sandboxed distribution.** Inside a Flatpak, creating a *sink* requires the
   `--socket=pipewire` permission and a PipeWire ≥ 0.3.50 host; writing the `default` metadata may be
   restricted by the portal's access rules in future versions. Test early; if metadata writes are
   denied, fall back to instructing the user to pick FxSound in their sound settings.

10. **PipeWire version floor.** `target.object` (as opposed to the deprecated `node.target`) needs
    ≥ 0.3.64; `node.link-group` semantics settled around 0.3.43. **Declare a hard minimum of
    PipeWire 0.3.65 and WirePlumber 0.4.14**, detect at startup via `pw_get_library_version()`, and
    refuse with a clear message below it rather than half-working.

11. **No global hotkeys under Wayland.** The Windows build registers system-wide hotkeys; a Wayland
    client cannot. The replacement is (a) a D-Bus interface the user binds to a compositor shortcut,
    (b) the XDG `GlobalShortcuts` portal where the compositor implements it (GNOME 45+/KDE 6), or
    (c) MPRIS2 for play/pause-adjacent actions. **None of these is a drop-in**; the UX needs design
    work, and it does not belong in this subsystem — flagged here only because
    `AudioPassthru`-adjacent code in `FxController` registers them.

12. **Startup ordering.** The Windows app can create its sink before any device exists because the
    driver is always present. Ours must handle "PipeWire is up but no sinks yet" (early boot, or a
    dock still enumerating). NODE 1 should be created regardless — apps can render into it with no
    output attached, and NODE 2 connects when a target appears. **Verify this does not cause a
    client-visible xrun storm**; if it does, output silence from NODE 2 into a dummy target rather
    than leaving it unconnected.

---

## 28. Linux input mode: FxSound behind a microphone

> **Scope.** Linux only. The Windows build sits exclusively in front of a *playback* endpoint;
> `audiopassthru/` has no capture-device mode and `SoundDevice::isCaptureDevice`
> (`AudioPassthru.h:34`) marks the FxSound endpoint itself, not a microphone. This section is the
> authority for the port's second direction, implemented in `crates/fxsound-audio/src/engine.rs`
> (`build_nodes`, `switch_direction`, `claim_default`, `release_default`) and
> `crates/fxsound-audio/src/devices.rs` (`choose_device`, `restore_default_candidate`).

### 28.1 One engine, one direction at a time

`fxsound_core::DeviceDirection { Output, Input }` is the axis. The engine starts in `Output` — the
Windows behaviour of §19 — and enters `Input` when the GUI, the tray or `--output=<node.name>` names
a capture device (`UiToAudio::SelectDevice { node_name, direction: Input }`). It never runs both
pairs of nodes at once: switching direction destroys the active pair first, so the system only ever
sees **one** FxSound device — "FxSound (Output)" under its sinks *or* "FxSound (Input)" under its
sources, never both. That is what makes "the FxSound output sink disappears when a microphone is
selected" hold without any extra hiding logic.

Everything in §19.2 (DSP placement), §19.3 (format policy), §19.6 (the two `process()` callbacks),
§23 (RT constraints) and §24 (latency) applies verbatim: the input pair reuses the two callbacks and
the ring unchanged. Only properties, targets, metadata keys and bookkeeping differ.

### 28.2 Topology and exact node properties

```
 ┌────────────────────────────────────────────────────────────────────────────┐
 │  alsa_input.usb-…-analog-stereo   (the real microphone, Audio/Source)      │
 └──────────────────────────────────┬─────────────────────────────────────────┘
                                    │  link created by WirePlumber because
                                    │  target.object = "<chosen source node.name>"
                                    ▼
 ╔══════════════════════════════════════════════════════════════════════════╗
 ║  NODE 1  "fxsound_capture"        media.class = Stream/Input/Audio         ║
 ║  pw_stream, direction = Input,    node.link-group = "fxsound"              ║
 ║   process():  dequeue → DSP in place → push to ring   (== §19.6 NODE 1)   ║
 ╚══════════════════════════════════╤═══════════════════════════════════════╝
                                    │  the same SPSC ring as §19.1
 ╔══════════════════════════════════▼═══════════════════════════════════════╗
 ║  NODE 2  "fxsound_source"         media.class = Audio/Source               ║
 ║  pw_stream, direction = Output,   node.link-group = "fxsound"              ║
 ║   process():  dequeue → pop from ring (zero-fill on underrun) → queue      ║
 ╚══════════════════════════════════╤═══════════════════════════════════════╝
                                    │  links created by WirePlumber because
                                    │  default.configured.audio.source == "fxsound_source"
        ┌───────────────────────────┴───────────────────────────┐
 ┌──────┴───────┐  ┌──────────────┐  ┌──────────────┐           │
 │ Discord      │  │ OBS          │  │ pw-record    │   ← Stream/Input/Audio nodes
 └──────────────┘  └──────────────┘  └──────────────┘
```

**NODE 1 — the capture stream** (the mirror of §20 NODE 2):

| Key | Value | Why |
| --- | --- | --- |
| `media.class` | `"Stream/Input/Audio"` | An ordinary capture stream. |
| `media.category` | `"Capture"` | |
| `media.role` | `"Production"` | Not `Communication`: role policies must not cork or duck the thing every recorder is recording through. |
| `media.name` | `"FxSound"` | |
| `node.name` | `"fxsound_capture"` | Fixed ASCII; never listed as a device (`OUR_NODE_NAMES`). |
| `node.description` | `"FxSound capture"` | Internal node; not localised. |
| `node.link-group` | `"fxsound"` | **Mandatory** — see 28.3. |
| `target.object` | the chosen source's `node.name` | |
| `stream.capture.sink` | `"false"` | Capture the microphone itself, not a sink monitor. |
| `node.autoconnect` / `node.dont-reconnect` / `node.passive` | `"true"` / `"false"` / `"false"` | As NODE 2 of §20. |
| `node.latency`, `stream.dont-remix`, `application.*` | as §20 | |
| connect | `Direction::Input`, `AUTOCONNECT \| MAP_BUFFERS \| RT_PROCESS` | A capture stream *receives* audio. |

**NODE 2 — the virtual source** (the mirror of §20 NODE 1):

| Key | Value | Why |
| --- | --- | --- |
| `media.class` | `"Audio/Source"` | What `pw-loopback --playback-props=media.class=Audio/Source` uses to make a virtual microphone out of a `pw_stream`. **Not** `Audio/Source/Virtual`, which is the null-source factory's class. |
| `media.type` | `"Audio"` | |
| `node.name` | `"fxsound_source"` | The stable ID written into the default metadata. |
| `node.description` / `node.nick` | `"FxSound (<Input>)"`, localised — 28.4 | |
| `node.virtual` | `"true"` | |
| `node.link-group` | `"fxsound"` | **Mandatory** — see 28.3. |
| `node.want-driver` | `"true"` | Scheduled by whatever driver is running even before a recorder links to it. |
| `node.always-process` | `"false"` | |
| `audio.channels` / `audio.rate` / `audio.format` / `audio.position` | as §20 NODE 1: `clamp(source.channels, 2, 8)`, graph rate, `F32`, the source's positions | A **mono microphone is accepted**: the stream declares `2` and PipeWire's adapter up-mixes. `SND_DEVICES_MONO_BUG_SKIP_MONO_DEVICES` was a playback-driver workaround and does not apply to capture. |
| `device.class`, `priority.session` = `500`, `priority.driver` = `0`, `monitor.channel-volumes`, icons | as §20 NODE 1 | Never wins the default implicitly (§21, open question 2). |
| connect | `Direction::Output`, `MAP_BUFFERS \| RT_PROCESS` — **no** `AUTOCONNECT` | A source does not connect anywhere; recorders connect *to* it. |

The format is decided once per `build_nodes` and declared on both nodes, exactly as §19.3.

### 28.3 Why the link-group still matters

Once `fxsound_source` is the default source, any capture stream without an explicit target is
linked to it by WirePlumber — including, without the group, our own `fxsound_capture` the moment
its microphone goes away and `node.dont-reconnect = false` sends it looking for a new target. With
both nodes in link-group `fxsound`, `linking-utils.lua`'s `canLinkGroupCheck` refuses that link, so
the capture stream can never be fed by our own source. WirePlumber's `find-best-default-node.lua`
only excludes *smart* filters (`filter.smart = true`) from being default, so a plain link-group node
is still allowed to become `default.audio.source` — which is what we need.

### 28.4 Descriptions in the system language

`node.description` and `node.nick` of the two virtual nodes are `"FxSound (<word>)"`, where
`<word>` is "Output" / "Input" translated into the system message locale — `LC_ALL`, then
`LC_MESSAGES`, then `LANG`, language part only, `C`/`POSIX` and unknown languages falling back to
English (`crates/fxsound-audio/src/locale.rs`). On the reference machine (`ru_RU.UTF-8`) that is
"FxSound (Вывод)" and "FxSound (Ввод)", which is how `pavucontrol` and the desktop widget tell the
two apart — the same way ALSA/UCM already localise "Analog Stereo" into "Аналоговый стерео" there.
Node **names** are never localised: they are matched by string and written into metadata.

### 28.5 Device selection, per direction

`choose_device(devices, direction, our_node, current_default, previous_names, memory)` is §19.5
run over the devices of one direction with `our_node = fxsound_source` and `current_default =
default.audio.source`. Differences from the output run:

* rule 1 ends in `NoInputDevices` rather than `NoOutputDevices`;
* rule 5 accepts a newly plugged mono microphone;
* the mono guard (`-57`/`-58`) does not run.

One deviation applies to **both** directions: rule 2 (first run adopts the current default) yields
to an explicit `user_selected` device that is present. Windows never wrote `user_selected` (open
question 8) so the two could not conflict there; on Linux the first rules run of the input direction
happens *because* the user picked a microphone, and rule 2 would otherwise attach FxSound to
whatever WirePlumber had as the default source instead.

`SelectionMemory` (§19.5's five registry slots) exists **once per direction**, so trying a
microphone never forgets which speakers the user had, and `pwszIDPreviousRealDevices` is cleared on
every direction switch so rule 5 cannot treat every device of the new direction as freshly plugged.

`--output NAME` matches a device of either direction (node name first, then description);
`--next-output` cycles only within the selected device's direction so a compositor keybind never
flips the mode by accident.

### 28.6 The default source, politely — and the order of teardown

§21 applies with `sink` → `source` throughout:

```
keys:    default.audio.source              (in effect now — WirePlumber's, read only)
         default.configured.audio.source   (the user's choice — the ONLY key FxSound writes)
value:   Spa:String:JSON  {"name":"fxsound_source"}
```

The engine **takes the default automatically** for its active direction as soon as its pair of
nodes is up (`want_default` is `true` unless the GUI sent `SetAsDefault(false)`), after remembering
the previous default: the first of `default.configured.audio.*`, `default.audio.*` that is not one
of our own names goes into `most_recent_default` (and `original_default` once). A stale configured
value that already names us — left behind by a `SIGKILL`ed FxSound, which WirePlumber's state file
preserves — is deliberately skipped so the memory never says "the default before us was us".

The default is **released before the nodes are destroyed**, always, on each of these paths:

| Path | Function | What happens, in order |
| --- | --- | --- |
| clean exit | `engine::run` tail | `release_all_defaults` → drop the session (nodes, metadata proxy) |
| direction switch | `switch_direction` | `release_default(old)` → drop nodes → set direction → re-run rules → build → `claim_default(new)` |
| rules end in an error (no devices, `-57`, `-58`) | `apply_rules` | `release_default(active)` → drop nodes → report |
| reconnect / restart / NODE 1 error | `disconnect` | `release_all_defaults` → drop session → backoff → reconnect → claim again |
| `SetAsDefault(false)` | `handle_control` | `release_default(active)`, nodes untouched |

Release writes `restore_default_candidate(memory, devices, direction)` — `user_selected` →
`most_recent_playback` → `most_recent_default` → `prior_default` → `original_default`, first one
*present in that direction* — or leaves the key alone when nothing remembered is present.

Two rebuild paths deliberately **keep** the default, because the same node is about to reappear
under the same name and `default.configured.audio.*` survives the gap: a target change within the
same direction, and the supervisor's format-mismatch / NODE 2-error rebuilds.

### 28.7 What the GUI receives

`AudioToUi::Devices` carries both directions, grouped: every output sorted by description, then
every input sorted by description, each with `is_default` judged against the default *of its own
direction*. Mono outputs are omitted (they could never be chosen); mono inputs are listed. The tray
draws the same list as two radio groups under disabled "Output" / "Input" header rows, and its
tooltip's second line reads `Output: …` or `Input: …` after the selected device's direction.
