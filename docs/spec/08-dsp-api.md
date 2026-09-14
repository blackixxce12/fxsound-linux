# 08 — DfxDsp engine: public surface and private core

> Reverse-engineering spec for the Rust/egui/PipeWire port of FxSound.
> Every number below is cited as `path:line` against the tree at
> `/home/blackixxce/Загрузки/fxsound-app-main`. Paths are repo-relative.

---

## 0. Scope and source map

The DSP subsystem is a **static library** (`dsp/DfxDsp.vcxproj`, `dsp/ReadMe.txt:1-3`)
exposing exactly one C++ class, `DfxDsp`, through one header
(`dsp/include/DfxDsp.h`). Everything else is private.

| Layer | Files | Role |
|---|---|---|
| Public façade | `dsp/include/DfxDsp.h`, `dsp/DfxDsp.cpp` | 27 methods, pure forwarding to pimpl |
| Pimpl | `dsp/u_DfxDsp.h`, `dsp/DfxDspPrivate.cpp`, `dsp/DfxDspEq.cpp`, `dsp/DfxDspPreset.cpp`, `dsp/DfxDspRegistry.cpp` | state cache + adapter onto legacy `dfxp` API |
| Legacy engine ("dfxp") | `dsp/ptutil/dfxp/*.cpp`, `dsp/ptutil/dfxp/u_dfxp.h` | format tracking, parameter marshalling, registry I/O, spectrum |
| Parametric EQ + gain stages | `dsp/ptutil/DspUtil/GraphicEq/*`, `dsp/ptutil/SOS/*`, `dsp/ptutil/Filt/*` | 31-band EQ, master gain, balance, normalization, volume leveling |
| DSP kernels | `dsp/ptComSftDfx/Comsftwr.c`, `dsp/ptutil/COM/Comwave.cpp`, `dsp/ptechDsp/**` | the actual five effects (`ply0` graph) |
| Analyser | `dsp/ptutil/DspUtil/spectrum/*` | 10-band VU / spectrum |

`dsp/ReadMe.txt` is a stock Visual Studio "STATIC LIBRARY : DfxDsp Project Overview"
AppWizard boilerplate (`dsp/ReadMe.txt:1-29`) and contains **no engineering
information**. Do not port anything from it.

### Return-code convention

All `int`-returning methods use `OKAY == 0` (`dsp/ptutil/include/codedefs.h:95`) and
`NOT_OKAY == 1` (`dsp/ptutil/include/codedefs.h:139`; in debug/release builds
`NOT_OKAY` is a macro that also logs file+line, `codedefs.h:108`/`:128`). Booleans
use `IS_TRUE`/`IS_FALSE`. In Rust, model this as `Result<(), DspError>`.

---

## 1. Runtime topology (who calls what, on which thread)

```
 ┌─────────────────────────── GUI thread (JUCE message thread) ──────────────────────────┐
 │ FxController  ──► DfxDsp::setEffectValue / setMasterGain / setBalance /                │
 │                    setNormalization / setVolumeLeveling / setFilterQ /                 │
 │                    setNumBands / setEqBand*/ eqOn / powerOn / loadPreset               │
 │ FxController::timerCallback() @100 ms ──► DfxDsp::getTotalAudioProcessedTime()         │
 │ FxVisualizer   @repaint      ──► DfxDsp::getSpectrumBandValues(buf, 10)                │
 └────────────────────────────────────────────────────────────────────────────────────────┘
                                        ▲  (Windows registry = the IPC bus)
                                        │
 ┌────────────────── AudioPassthru worker thread (THREAD_PRIORITY_TIME_CRITICAL) ────────┐
 │ while(1) { WASAPI capture → DfxDsp::setSignalFormat(...)                               │
 │                          → DfxDsp::processAudio(buf, buf, frames, 0)                   │
 │                          → WASAPI render }                                             │
 └────────────────────────────────────────────────────────────────────────────────────────┘
```

* Audio call site: `audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:533`
  (`setSignalFormat`) and `:551` (`processAudio`). Thread priority is raised at
  `audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:493`.
* `i_check_for_duplicate_buffers` is hard-coded `IS_FALSE` by the only caller
  (`audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:528`).
* `setSignalFormat` is called **once per buffer**, i.e. on the audio thread, and is
  cheap only because it early-outs on unchanged format.
* GUI polling: `fxsound/Source/GUI/FxController.cpp:2062` (`getTotalAudioProcessedTime`)
  and `fxsound/Source/GUI/FxController.cpp:2891` (`getSpectrumBandValues`).

**Critical architectural fact:** there is *no* lock-free parameter queue. The GUI thread
writes parameters straight into the shared `dfxpHdlType` struct **and into the Windows
registry**, and the audio thread reads them back out of the registry, per buffer. See
§12.

---

## 2. Public API reference

Declared in `dsp/include/DfxDsp.h:35-77`. Every method is a one-line forward to
`DfxDspPrivate` (`dsp/DfxDsp.cpp:24-201`), except `setSignalFormat` (`dsp/DfxDsp.cpp:95`)
and `processAudio` (`dsp/DfxDsp.cpp:106`) which first test `data_->being_destroyed_`
and return `OKAY` without touching the engine if the pimpl is being torn down
(`dsp/DfxDsp.cpp:97-103`, `:108-115`; flag set in `dsp/DfxDspPrivate.cpp:121`).

### 2.1 Types

```cpp
struct DfxPreset { std::wstring full_path; std::wstring name; };      // DfxDsp.h:29-32
enum Effect { Fidelity=0, Ambience=1, Surround=2, DynamicBoost=3,
              Bass=4, NumEffects=5 };                                 // DfxDsp.h:38
```

The enum order is load-bearing: it indexes the GUI slider array and is cast straight
from `FxEffects::EffectType` (`fxsound/Source/GUI/FxController.cpp:1763`). **Preserve
0..4 exactly.**

### 2.2 Method table

| Method | Units | Valid range | Default | RT-safe? | Side effects |
|---|---|---|---|---|---|
| `DfxDsp()` | — | — | — | no | `new DfxDspPrivate` (`DfxDsp.cpp:26`) → `dfxpInit`, `calloc` of ~2 MB handle, 5× `comInit`, EQ, spectrum, registry reads/writes |
| `~DfxDsp()` | — | — | — | no | `delete data_` (`DfxDsp.cpp:32`) |
| `int setSignalFormat(int bps,int nch,int srate,int valid_bits)` | bits, count, Hz, bits | bps ∈ {8,16,24,32}; nch 1..8; srate 16000..192000; valid_bits ≤ bps | 16 / 2 / 44100 / 16 | **no** (on change) | see §5 |
| `int processAudio(short* in, short* out, int frames, int check_dup)` | — | frames ≥ 0 | — | **no** (see §12) | in-place DSP, spectrum, processed-time counter |
| `int loadPreset(wstring path)` | — | `.fac` vals file | — | no | file I/O, writes 31 EQ bands to registry |
| `int savePreset(wstring name, wstring path)` | — | — | — | no | file I/O; appends `.fac` (`DfxDspPreset.cpp:106`) |
| `int exportPreset(wstring src, wstring name, wstring dst)` | — | — | — | no | reads src, **applies it to the live engine** (`DfxDspPreset.cpp:124`), writes dst |
| `void eqOn(bool)` | — | — | true (registry default, `dfxpEq.cpp:164`) | no | registry write + arms `update_from_registry_` |
| `int getNumEqBands()` | count | 1..31 | 31 | yes (plain read) | none |
| `float getBalance()` / `void setBalance(float)` | dB | GUI clamps −20..+20, step 1 | 0.0 | no | see §8.2 |
| `float getNormalization()` / `void setNormalization(float)` | dB | unclamped; sane −40..0 | 0.0 | no | see §8.3 |
| `float getVolumeLeveling()` / `void setVolumeLeveling(float)` | **unitless 0..4** (misnamed `gain_db`) | clamped 0..4 | 0.0 | no | see §8.4 |
| `float getMasterGain()` / `void setMasterGain(float)` | dB | GUI clamps −20..+20, step 2 | 0.0 | no | see §8.1 |
| `float getFilterQ()` / `void setFilterQ(float)` | multiplier | GUI clamps 1..3, step 0.5 | 1.0 | no | rebuilds every EQ section |
| `void setNumBands(int)` | count | 1..31 | 31 | no | rebuilds EQ; **mutates the process-global `DFXP_GRAPHIC_EQ_NUM_BANDS`** |
| `float getEqBandFrequency(int band)` | Hz | band 0-based | per table §8.6 | yes | none |
| `void setEqBandFrequency(int band, float hz)` | Hz | clamped 10..21000 | — | no | recalcs that section |
| `void getEqBandFrequencyRange(int band, float* lo, float* hi)` | Hz | — | — | yes | writes `*lo=*hi=0` first (`DfxDspEq.cpp:483-484`) |
| `float getEqBandBoostCut(int band)` | dB | −12..+12 stored | 0.0 | yes | none |
| `void setEqBandBoostCut(int band, float dB)` | dB | clamped ±12 | 0.0 | no | registry write (`%.2f`) + memory |
| `void powerOn(bool)` | — | — | on (bypass default `IS_FALSE`, `dfxpGet.cpp:179`) | no | registry write `byAll` + full re-communicate |
| `bool isPowerOn()` | — | — | — | no | **INVERTED — see §14.1** |
| `float getEffectValue(Effect)` | **0.0..1.0** | — | 0.0 (stale, see §14.2) | yes | returns `-1.0f` for bad enum (`DfxDspPrivate.cpp:251`) |
| `void setEffectValue(Effect, float)` | **0..10** | 0..10 | see §9.1 | no | registry write + re-communicate |
| `DfxPreset getPresetInfo(wstring path)` | — | — | — | no | file read; **ignores all errors** |
| `unsigned long getTotalAudioProcessedTime()` | **milliseconds** | wraps at 2^32 | 0 | yes | plain read of `ul_total_msecs_audio_processed_time` |
| `void resetTotalAudioProcessedTime()` | — | — | — | yes | sets counter to 0 (`DfxDspPrivate.cpp:318`) |
| `void getSpectrumBandValues(float*, int n)` | linear 0.0..1.0 | **n must == 10** | 0.0 | yes | copies 10 floats; **no-op if `n != 10`** |

---

## 3. Construction, defaults and teardown

`DfxDspPrivate::DfxDspPrivate()` (`dsp/DfxDspPrivate.cpp:53-115`):

| Item | Value | Cite |
|---|---|---|
| Registry product name | `L"DFX"` | `DfxDspPrivate.cpp:38`, `:60` |
| Displayed product name | `L"FxSound"` | `DfxDspPrivate.cpp:39` |
| `full_version` | `13.028f` | `DfxDspPrivate.cpp:62` |
| `major_version` | `13` (int cast of 13.028) | `DfxDspPrivate.cpp:63` |
| Vendor code | `23` (`DFXP_VENDOR_CODE_UNIVERSAL`) | `DfxDspPrivate.cpp:40`, `:64` |
| `dfxpInit` args | product `L"DFX"`, vendor 23, trial days 14, extend 1, freemium FALSE, allow_remix FALSE, host_buffer_delay 0 ms, oem FALSE, **processing_only FALSE**, trace FALSE, slout `nullptr` | `DfxDspPrivate.cpp:68-74` vs prototype `dsp/ptutil/include/dfxp.h:55` |
| Vocal reduction | forced OFF at construction | `DfxDspPrivate.cpp:81` |
| MIDI↔real quantisers | 0..127 ↔ 0.0..1.0, `QNT_RESPONSE_LINEAR` | `DfxDspPrivate.cpp:91-112`; `MIDI_MIN_VALUE 0`/`MIDI_MAX_VALUE 127` at `DfxDspPrivate.cpp:50-51`; `DFX_UI_MIN_VALUE 0.0`/`DFX_UI_MAX_VALUE 1.0` at `dsp/ptutil/include/DfxSdk.h:92-93` |

`processing_only = IS_FALSE` ⇒ inside the engine `b_lean_and_mean = TRUE`
(`dsp/ptutil/dfxp/dfxpUniversal.cpp:128-132`, `dsp/ptutil/dfxp/dfxpProcessReal.cpp:80-84`).
This disables four code paths in the shipping configuration — see §6.4. **The Rust port
only needs the lean-and-mean paths.**

Engine-level defaults set by `dfxpInit` (`dsp/ptutil/dfxp/dfxpInit.cpp:104-131`):

| Field | Value | Constant | Cite |
|---|---|---|---|
| `sampling_freq` | 44100.0 | `DAW_INIT_SAMPLING_FREQ` | `dsp/ptutil/dfxp/u_dfxp.h:43` |
| `bits_per_sample` | 16 | `DAW_INIT_BITS_PER_SAMPLE` | `u_dfxp.h:47` |
| `valid_bits` | 16 | `DAW_INIT_VALID_BITS` | `u_dfxp.h:50` |
| `num_channels_in/out` | 2 | `DAW_INIT_NUM_CHANNELS` | `u_dfxp.h:51` |
| `internal_rate_ratio` | 1 | — | `dfxpInit.cpp:108` |
| `binaural_headphone_on_flag` | `IS_FALSE` | — | `dfxpInit.cpp:174` |
| `ul_total_msecs_audio_processed_time` | 0 | — | `dfxpInit.cpp:202` |
| `processing_override` | `DFXP_PROCESSING_OVERRIDE_NONE` (0) | `dsp/ptutil/include/dfxpDefs.h:135` | `dfxpInit.cpp:89` |

Destructor `~DfxDspPrivate()` (`dsp/DfxDspPrivate.cpp:118-152`): sets `being_destroyed_`,
frees `preset_list_handle_` (**uninitialised — see §14.3**), the two qnt handles,
then `dfxpFreeAll()` + `free(dfxp_handle_)` and `delete slout1_`.
`dfxpFreeAll` (`dsp/DfxDspPrivate.cpp:324-551`) frees 22 qnt handles, 5 `com` handles,
the GraphicEq, SurroundSyn, spectrum, BinauralSyn and shared-memory handles.

---

## 4. Sample format assumptions (answer: **not** 16-bit)

The `short int*` in the signature is a **historical lie**. `dfxpUniversalModifySamples`
re-casts the pointer to `BYTE*` and dispatches on the *last set* `bps`
(`dsp/ptutil/dfxp/dfxpUniversal.cpp:165-166`, `:171-187`):

| `last_called_bps` | Path | Sample type in the buffer |
|---|---|---|
| 8, 16, 24 | `dfxpModifyShortIntSamples` (`dsp/ptutil/dfxp/dfxpProcessInt.cpp:46`) | packed integers; converted to f32 via `mthConvertIntBufToRealtype` (`dfxpProcessInt.cpp:93`) and back via `mthConvertRealtypeBufToIntBuf` (`dfxpProcessInt.cpp:124`) |
| 32 | in→out memcpy loop then `dfxpModifyRealtypeSamples` **in place** | `float` (`realtype`), nominal ±1.0 |
| anything else | *silently does nothing* — no copy, no processing | — |

In the shipping FxSound topology the format is **always 32-bit float**
(`audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:548`: *"Format will always be
32 bit floating point"*, and `dfxpUniversal.cpp:63-65`: *"call is always set up for 32 bit
processing since conversion is done before and after processing calls"*).

* `realtype` is `float` throughout (32-bit), NOT double.
* Channel layout is **interleaved**, Windows/WAVE order: 5.1 = FL FR FC LFE BL BR;
  7.1 = FL FR FC LFE BL BR SL SR (`dsp/ptutil/dfxp/dfxpProcessReal.cpp:218-221`).
* `i_num_sample_sets` = **frames** (sample *sets*), not samples. Total samples =
  `frames * nch`.

**Linux/PipeWire equivalent:** take `&mut [f32]` interleaved in the node's negotiated
`SPA_AUDIO_FORMAT_F32` / `F32P` (planar) layout. Negotiate `F32` interleaved to match
this code 1:1; do the int↔float paths only if you also want to accept S16/S24 nodes —
PipeWire will convert for you, so **drop the 8/16/24-bit code paths entirely**.

---

## 5. `setSignalFormat` — exact contract

```cpp
int DfxDsp::setSignalFormat(int i_bps, int i_nch, int i_srate, int i_valid_bits);
```

1. If `being_destroyed_` → return `OKAY`, no-op (`dsp/DfxDsp.cpp:97`).
2. `dfxpUniversalSetSignalFormat` (`dsp/ptutil/dfxp/dfxpUniversal.cpp:48`) compares all
   four against `universal.last_called_{bps,nch,srate,valid_bits}`. **If all four are
   unchanged it returns `OKAY` immediately** (`dfxpUniversal.cpp:58-76`). This is what
   makes per-buffer calls affordable. The cached values start at 0 (the handle is
   `calloc`'d, `dfxpInit.cpp:59`), so the *first* call always reconfigures.
3. On change it stores the new quadruple and calls `dfxpBeginProcess(bps, nch, srate)`
   (`dsp/ptutil/dfxp/dfxpProcess.cpp:46`) then `dfxpSetValidBits(valid_bits)`
   (`dfxpProcess.cpp:162`). Order matters: `dfxpBeginProcess` resets `valid_bits = bps`
   (`dfxpProcess.cpp:93`), so `dfxpSetValidBits` must run after.

### 5.1 What `dfxpBeginProcess` does

```
bits_per_sample   = bps
valid_bits        = bps                       (dfxpProcess.cpp:93)
num_channels_in   = num_channels_out = nch    (dfxpProcess.cpp:94-95)
sampling_freq     = srate
internal_sampling_freq = srate                (dfxpProcess.cpp:97)
sampling_period   = 1/srate
internal_rate_ratio = 1                       (dfxpProcess.cpp:100)
```

Then the **internal-rate reduction** (`dfxpProcess.cpp:104-118`):

| Input rate | internal rate | `internal_rate_ratio` |
|---|---|---|
| `srate <= 48000` | `srate` | 1 |
| `48000 < srate < 192000` (88.2k, 96k) | `srate / 2` | 2 |
| `srate >= 192000` | `srate / 4` | 4 |

`DFXP_MAX_INTERNAL_SAMPLING_FREQ = 48000.0` (`u_dfxp.h:45`),
`DFXP_MAX_SAMPLING_FREQ = 192000.0` (`u_dfxp.h:44`),
`DFXP_MIN_SAMPLING_FREQ = 16000.0` (`u_dfxp.h:46`).

The ratio is applied by naive **decimate-by-N / hold-by-N** inside
`comProcessWaveBuffer` (`dsp/ptutil/COM/Comwave.cpp:124-146` down, `:166-204` up) — it
literally drops samples and then replicates the processed sample N times, with leftover
frames filled from the last processed value (`Comwave.cpp:179-187`). There is **no
anti-alias filtering**. This is audibly poor at 96/192 kHz and is a prime candidate for
replacement in the Rust port (see §16).

### 5.2 Unsupported-format handling

```
unsupported if:  srate > 192000  ||  srate < 16000
              ||  nch  > 8
              ||  bps  > 32      ||  bps  < 8          (dfxpProcess.cpp:121-127)
```
Sets `unsupported_format_flag = IS_TRUE` (`dfxpProcess.cpp:128`) and **returns `OKAY`**,
skipping qnt re-init and DSP reload. Consequence at process time:
* 32-bit path: in→out copy already happened, then `dfxpModifyRealtypeSamples` bails at
  `dfxpProcessReal.cpp:109` ⇒ **clean passthrough**.
* int path: explicit copy loop then return (`dfxpProcessInt.cpp:83-87`) ⇒ passthrough.

### 5.3 Side effects of a format change (all non-RT-safe)

* `dfxp_InitDynamicQnts` (`dfxpProcess.cpp:135`) — **frees and re-`calloc`s ~15 quantiser
  lookup tables**, each sized `128` entries; the bass-boost one is an array of 128
  fully-designed biquads (`dsp/ptutil/dfxp/dfxpQnt.cpp:530-538`,
  `dsp/ptutil/Qnt/QntitoBoostCut.cpp:47-51`).
* `dfxp_ComLoadAndRun` (`dfxpProcess.cpp:139`, def `dsp/ptutil/dfxp/dfxpComm.cpp:128`) —
  reloads `ply0` for all five `com` handles with the new internal rate and stereo flag,
  which runs `DSPS_PLAY_INIT` with `DSPS_INIT_PARAMS | DSPS_INIT_MEMORY`, i.e.
  **`malloc`/`realloc` of the DSP delay memory** (`dsp/ptComSftDfx/Comsftwr.c:846`, `:863`)
  and a full state zeroing (`dsp/ptechDsp/Play/Play32/Play32.c:201-252`).
* `dfxpCommunicateAll` (`dfxpProcess.cpp:141`) — re-marshals every parameter, which
  performs **~12 Windows-registry reads** (see §12).

**Therefore: in the Rust port `prepare()` must be a separate, non-realtime call**, and
`process()` must never be able to trigger it. If the graph format changes, PipeWire
re-negotiates and calls your `param_changed`/`reconfigure` on a non-RT thread — do the
reallocation there, and have `process()` fall back to passthrough for one buffer if a
reconfigure is pending.

---

## 6. `processAudio` — exact contract

```cpp
int DfxDsp::processAudio(short int *in, short int *out, int frames, int check_dup);
```

### 6.1 Buffer contract

* `in` and `out` may be — and in FxSound **always are** — the **same pointer**
  (`audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:551`).
* For `bps == 32` the code explicitly copies `in → out` first and then processes `out`
  **in place** (`dsp/ptutil/dfxp/dfxpUniversal.cpp:178-186`), so aliasing is safe and a
  distinct `out` also works.
* `frames` is the number of interleaved **sample sets**. Required buffer size:
  `frames * nch * bps/8` bytes (`dfxpUniversal.cpp:135`).
* Nothing validates `frames` against the buffer; the caller must be honest.

```
 in/out buffer, nch = 2, bps = 32, frames = N
 ┌──────┬──────┬──────┬──────┬─────┬────────┬────────┐
 │ L[0] │ R[0] │ L[1] │ R[1] │ ... │ L[N-1] │ R[N-1] │   4 bytes each
 └──────┴──────┴──────┴──────┴─────┴────────┴────────┘
   total_bytes = N * 2 * 4
```

### 6.2 Chunking

`DAW_MAX_BUFFER_SIZE = 16384` frames (`dsp/ptutil/dfxp/u_dfxp.h:38`).

```
if frames > 16384:
    process_count       = frames / 16384            (dfxpUniversal.cpp:154)
    leftover            = frames - count*16384      (dfxpUniversal.cpp:155)
    num_process_loop    = 16384
else:
    process_count = 1; leftover = 0; num_process_loop = frames
```
The loop advances `bp_in`/`bp_out` by `num_process_loop * nch * bps/8` bytes per
iteration (`dfxpUniversal.cpp:167`, `:189-190`), then handles the remainder
(`dfxpUniversal.cpp:193-212`).

Note the belt-and-braces second check inside `dfxpModifyRealtypeSamples`:
`if (frames > DAW_MAX_BUFFER_SIZE) return OKAY` (`dfxpProcessReal.cpp:109`) — unreachable
given the chunking, but it means a hypothetical direct call with a huge buffer is a
passthrough rather than a heap overflow.

Internal scratch buffers are fixed size:
`DFXP_SAMPLE_BUFFER_SIZE = DAW_MAX_BUFFER_SIZE * 8 = 131072` floats each for
`r_samples` and `r_samples_reordered` (`u_dfxp.h:56`, `u_dfxp.h:231-232`) — i.e. 512 KB
each, plus `universal.f_samples[131072]` (`u_dfxp.h:187`). That is ~1.5 MB of the handle,
all `calloc`'d once at init.

### 6.3 The duplicate-buffer check

```cpp
if (!b_lean_and_mean) {
    if (i_check_for_duplicate_buffers) {
        dfxp_UniversalCalcBufferHash(..., &hash_queue_vals[hash_queue_index]);
        hash_queue_index = (hash_queue_index + 1) % 10;
    }
}
```
(`dsp/ptutil/dfxp/dfxpUniversal.cpp:215-234`; queue size
`DFXP_UNIVERSAL_HASH_QUEUE_SIZE = 10`, `u_dfxp.h:103`.)

The hash is FNV-1a over the **output** bytes with a final avalanche
(`dfxpUniversal.cpp:386-425`):

```
p    = 16777619
hash = (int)2166136261
for each byte b:  hash = (hash ^ b) * p
hash += hash << 13;  hash ^= hash >> 7;  hash += hash << 3;
hash ^= hash >> 17;  hash += hash << 5;
if all bytes were zero: hash = 0            (dfxpUniversal.cpp:419-420)
```

**Three facts the porter must know:**
1. `b_lean_and_mean` is `TRUE` in FxSound, so the block **never runs**.
2. The only caller passes `check_dup = IS_FALSE` anyway
   (`audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp:528`).
3. `hash_queue_vals` is **written but never read anywhere in the tree**.

⇒ **Do not port the duplicate-buffer check.** Keep the parameter in the Rust signature
only if you want ABI parity; otherwise drop it. If you ever *do* need duplicate-buffer
detection (its original purpose was detecting a player that re-submits the same buffer
through both winmm and dsound hooks), on Linux the problem does not exist: a PipeWire
filter node sees each buffer exactly once.

### 6.4 Everything `processAudio` does, in order

```
DfxDsp::processAudio                                   DfxDsp.cpp:106
└─ if being_destroyed_ → return OKAY
└─ DfxDspPrivate::processAudio                         DfxDspPrivate.cpp:181
   ├─ processTimer()                                   DfxDspPrivate.cpp:183  ◄── !! see §12
   │   └─ if update_from_registry_:
   │       eqUpdateFromRegistry()   → 1 + 31 registry reads   DfxDspEq.cpp:48-107
   │       update_from_registry_ = false
   │       if anything changed → dfxpCommunicateAll()  DfxDspPrivate.cpp:177
   └─ dfxpUniversalModifySamples()                     dfxpUniversal.cpp:89
      ├─ b_lean_and_mean = !processing_only = TRUE
      ├─ bytes_total  = frames*nch*bps/8
      ├─ i_reorder    = (nch > 2)                      dfxpUniversal.cpp:140-143
      ├─ chunk loop (≤16384 frames) → per chunk:
      │   • bps 8/16/24 → dfxpModifyShortIntSamples
      │   • bps 32      → copy in→out, dfxpModifyRealtypeSamples(out, n, reorder)
      ├─ [skipped: duplicate-buffer hash]              dfxpUniversal.cpp:215
      ├─ dfxp_UniversalIsBufferAllSilence(in,…)        dfxpUniversal.cpp:241 / def :266
      └─ if !silent → dfxp_UniversalUpdateTotalTimeProcessed(frames)  :248 / def :330
```

Paths **disabled** by `b_lean_and_mean == TRUE`:

| Skipped | Where |
|---|---|
| `dfxp_UpdateBufferLengthInfo` (registry read+write per buffer!) | `dfxpProcessReal.cpp:95-103` |
| `dfxp_ClearBuffersIfSongStart` (would call `dfxpBeginProcess`) | `dfxpProcessReal.cpp:116-120` |
| `realSampleForceLegalValues_ArrayOnly` (clamp pass) | `dfxpProcessReal.cpp:353-357` |
| duplicate-buffer hash | `dfxpUniversal.cpp:215-234` |

Note the consequence of skipping the clamp pass: **nothing clamps the float buffer to
±1.0 before the effects run.** Only the volume-leveling stage clamps, and only when it is
enabled (`dsp/ptutil/SOS/SosProcess.cpp:693-697`), and the maximizer's own ceiling
(`max_output`, §9.4).

### 6.5 Silence detection and the processed-time counter

`dfxp_UniversalIsBufferAllSilence` (`dfxpUniversal.cpp:266`):

* `i_max_index = frames * nch` (`dfxpUniversal.cpp:289`).
* If `valid_bits < 32`: treats the buffer as `BYTE*` and tests `bytes[i] != 0` for
  `i in 0..i_max_index` — **this is a bug**, it inspects only the first `frames*nch`
  *bytes*, i.e. half the buffer for 16-bit, a quarter for 32-bit ints
  (`dfxpUniversal.cpp:292-305`).
* If `valid_bits >= 32`: tests `fabs(f[i]) > 1.0e-20` (`dfxpUniversal.cpp:312`). The
  comment explains why: Windows keeps streaming a ~6.36e-27 DC offset after playback
  stops (`dfxpUniversal.cpp:258-264`).

`dfxp_UniversalUpdateTotalTimeProcessed` (`dfxpUniversal.cpp:330`):

```
r_num_secs  = frames / last_called_srate                      :352
ul_msecs    = (long)(r_num_secs * 1000)      ← TRUNCATED      :353
if (!bypass_all) ul_total_msecs_audio_processed_time += ul_msecs   :358-361
shared_memory_total += ul_msecs               ← unconditional  :364-375
```

`getTotalAudioProcessedTime()` returns the *first* counter
(`dsp/ptutil/dfxp/dfxpGet.cpp:362`, via `DfxDspPrivate.cpp:307-314`).

**Truncation matters.** At 48 kHz with a 480-frame buffer, `0.01 s * 1000 = 10 ms` — fine.
With a 128-frame buffer, `128/48000*1000 = 2.667 → 2 ms`, a **25 % undercount**. The GUI
only uses the counter as a "did it change?" liveness probe
(`fxsound/Source/GUI/FxController.cpp:2062-2073`, 5 consecutive ticks at 100 ms to declare
"audio is processing"), so the inaccuracy is harmless *there* — but do not present this
number to the user as a play-time statistic. In Rust, accumulate frames as `u64` and
convert to ms only on read.

---

## 7. The signal chain — exact order

### 7.1 Top level (per buffer), `dfxpModifyRealtypeSamples`, `dfxpProcessReal.cpp:51`

```
float buffer (interleaved, in place)
   │
   ├─(1) bypass_all  ← registry read "byAll"                        :126
   │     dfx_tuned_track_playing ← registry read                    :136-139  (forces bypass)
   │
   ├─(2) GRAPHIC EQ / GAIN BLOCK   (nch ∈ {1,2,6,8} only)
   │      if !bypass && eq_on  → GraphicEqProcess(…)                :152
   │      if  bypass && eq_on  → GraphicEqProcess_MasterGainOnly()  :165
   │      if !eq_on            → block entirely skipped             :149,:162
   │
   ├─(3) BINAURAL (headphone virtualisation)
   │      only if binaural_headphone_on_flag && !bypass && srate<=48000   :176
   │      2ch → BinauralSynProcessStereoFormat                      :180
   │      6/8ch → BinauralSynProcessSurroundFormatWindowsOrdering    :187
   │      (flag is FALSE by default: dfxpInit.cpp:174, and is only set from
   │       DFX_UI_BUTTON_HEADPHONE whose registry default is "bypassed",
   │       dfxpGet.cpp:215-217)
   │
   ├─(4) DE-INTERLEAVE / REORDER for nch ∈ {4,6,8}                  :230-339
   │      into cast_handle->r_samples_reordered, grouped as
   │      [front LR interleaved][center mono][sub mono][rear LR][side LR]
   │      also computes center_nonzero / sub_nonzero / rear_nonzero / side_nonzero
   │      so silent surround channels can skip DSP entirely          :245-337
   │
   ├─(5) if !bypass_all: THE FIVE EFFECTS, per channel group        :347-471
   │      comProcessWaveBuffer(com_hdl_front, …, internal_rate_ratio, COM_32_BIT_FLOAT_SAMPLES)
   │      + _rear / _center / _subwoofer / _side as applicable
   │      (COM_32_BIT_FLOAT_SAMPLES == 1, dsp/ptutil/include/comSftwr.h:36)
   │
   ├─(6) SPECTRUM ANALYSER (always, bypassed or not)                :474-511
   │      spectrumProcess(rp_buf, frames, 2 (or 1 if mono in), srate, !bypass_all)
   │      every >1024 accumulated frames → publish to shared memory  :501-510
   │
   └─(7) RE-INTERLEAVE for nch ∈ {4,6,8}                            :514-572
```

**Ordering facts to preserve:**
* EQ + master gain + balance + normalization + volume leveling run **before** the five
  effects, not after.
* When the engine is bypassed, the EQ chain is replaced by a **master-gain-only** pass —
  so the master-gain slider still works with the power button off, but EQ, balance,
  normalization and volume leveling do not (`dfxpProcessReal.cpp:158-170`,
  `dsp/ptutil/SOS/SosProcess.cpp:501-516`).
* The spectrum is computed from the **post-EQ, post-effects** front-channel signal
  (`rp_buf` after step 5) and is fed zero input while bypassed
  (`dsp/ptutil/DspUtil/spectrum/spectrumProcess.cpp:73-82`).

### 7.2 Inside one `com` handle: the `ply0` graph

`comProcessWaveBuffer` (`dsp/ptutil/COM/Comwave.cpp:89`) → down-sample →
`comSftwrProcessWaveBuffer` (`dsp/ptComSftDfx/Comsftwr.c:109`) → `DSPS_PLAY_PROCESS`
(`dsp/ptechDsp/Play/Play32/Play32.c:423`) → up-sample.

```
DSPS_PLAY_PROCESS, in place on the float block          Play32.c:423
  if (s->bypass_on) → nothing at all                     :439
  else:
    (a) Vocal reduction     if vocal_elim_on && val!=0   :442   ← forced OFF, DfxDspPrivate.cpp:81
    (b) AURAL ACTIVATOR     if activator_on              :640   "Fidelity"
    (c) LEX REVERB          if ambience_on               :659   "Ambience"
    (d) WIDENER             if widener_on                :678   "Surround"
    (e) BASS BOOST          if bassboost_on              :695   inline biquad, local to Play32
    (f) HEADPHONE (xfeed + 8-tap delay) if headphone_on  :763   ← hard-wired OFF, dfxpComm.cpp:498
    (g) MAXIMIZER           ALWAYS, never bypassed       :872   "Dynamic Boost"
```

> `Play32.c:869-871`: *"Note that optimizer is never bypassed, the output gain is set to
> unity when the process switch on the UI is not selected."*

So the canonical five-effect order is:

**Fidelity → Ambience → Surround → Bass → Dynamic Boost**

which is **not** the enum order (`Fidelity, Ambience, Surround, DynamicBoost, Bass`).
Dynamic Boost is last (it is the limiter/maximizer); Bass is fourth.

### 7.3 Gain stages around the effects

| # | Stage | Gain | Cite |
|---|---|---|---|
| G1 | EQ section cascade output × `master_gain` | `10^(dB/20)`, default 1.0 | `SosProcess.cpp:583` (mono), `:630-631` (stereo) |
| G2 | × `balance_left` / `balance_right` (stereo only) | see §8.2 | `SosProcess.cpp:630-631` |
| G3 | × `normalization_gain` (stereo only, if `target_rms != 1.0`) | adaptive, clamped 0.01..1.0 | `SosProcess.cpp:677-723` |
| G4 | volume-leveling gain, ramped across the buffer | 0..`max_gain_cap`, hard clip at `effective_ceiling` | `SosProcess.cpp:725` → `applyVolumeLeveling` `:139` |
| G5 | Aural wet/dry: `dry_gain = 0.622047`, `wet_gain = 0.377953` | fixed | `Play32.c:266-267` |
| G6 | Lex wet/dry: `wet = 0.21*1.3 = 0.273`, `dry = 0.69*1.3 = 0.897` (warped at low settings) | see §9.2 | `Play32.c:297-298`, `dfxpComm.cpp:621-631` |
| G7 | Widener: `mono *= (1 − 0.3·intensity)`, `side *= (1 + 3·intensity)` | see §9.3 | `dsp/ptechDsp/wide/Wide32/Wide32.c:214-215` |
| G8 | Maximizer: `out = gain_boost · max_output · in`, then envelope limiter | `max_output = 0.966051` default | `dsp/ptechDsp/Maximizer/Maxi32/Maxi32.c:297`, `:91` |

There is **no output make-up or final clip** after the maximizer.

---

## 8. The GraphicEq / SOS block in detail

Handle: `struct GraphicEqHdlType` (`dsp/ptutil/DspUtil/GraphicEq/u_GraphicEq.h:38-66`)
wrapping `struct sosHdlType` (`dsp/ptutil/SOS/u_sos.h:46-97`).

Construction defaults (`dsp/ptutil/DspUtil/GraphicEq/GraphicEqInit.cpp:35-49`,
`dsp/ptutil/SOS/Sos.cpp:39-67`):

| Field | Default | Cite |
|---|---|---|
| `Q_multiplier` | 1 | `GraphicEqInit.cpp:49` |
| `master_gain` (dB cache) | 0.0 | `GraphicEqInit.cpp:51` |
| `normalization_gain` (dB cache) | 0.0 | `GraphicEqInit.cpp:52` |
| `volume_leveling_gain_db` (0..4 cache) | 0.0 | `GraphicEqInit.cpp:53` |
| `balance` (dB cache) | 0.0 | `GraphicEqInit.cpp:54` |
| `sos.master_gain` (linear) | 1.0 | `Sos.cpp:39` |
| `sos.balance_left/right` | 1.0 / 1.0 | `Sos.cpp:42-43` |
| `sos.target_rms` (normalization) | 1.0 = disabled | `Sos.cpp:46` |
| `sos.normalization_gain` | 1.0 | `Sos.cpp:47` |
| `sos.volume_leveling_target_rms` | 0.0 = disabled | `Sos.cpp:48` |
| `sos.volume_leveling_gain` | 1.0 | `Sos.cpp:49` |
| `sampling_freq` | 44100.0 (`GRAPHIC_EQ_DEFAULT_SAMPLING_FREQ`) | `dsp/ptutil/include/GraphicEq.h:39`, set at `GraphicEqInitSections.cpp:43` |
| band span | 20 Hz .. 20000 Hz | `GraphicEq.h:36-37`, `GraphicEqInitSections.cpp:48-49` |
| `SOS_MAX_NUM_SOS_SECTIONS` | 32 | `dsp/ptutil/include/sos.h:26` |

### 8.1 `setMasterGain(float gain_db)`

```cpp
cast_handle->master_gain = gain_db;                      // GraphicEqSet.cpp:99
float g = powf(10.0f, gain_db / 20.0f);                  // GraphicEqSet.cpp:101
sosSetMasterGain(sos, g);                                // → sos.master_gain
```
* **Units: dB.** Linear conversion `10^(dB/20)` (amplitude, not power).
* GUI range: `-20 .. +20`, step **2** (`fxsound/Source/GUI/FxAudioControls.cpp:312`);
  the controller additionally `std::round`s to integer dB
  (`fxsound/Source/GUI/FxController.cpp:1815`) and rejects out-of-band values from the
  command line with `if (mg < -20 || mg > +20) mg = DEFAULT_MASTER_GAIN`
  (`fxsound/Source/GUI/FxController.cpp:339`).
* Default `DEFAULT_MASTER_GAIN = 0.0f` (`fxsound/Source/GUI/FxController.h:51`).
* Applied per sample at `SosProcess.cpp:583` / `:630-631`, and it is the **only** gain
  applied when the engine is bypassed (`SosProcess.cpp:512-514`).
* Nothing clamps in the DSP layer — `setMasterGain(+60)` will be honoured.

### 8.2 `setBalance(float balance_db)`

```cpp
balance_left = 1.0; balance_right = 1.0;
if (balance_db > 0) balance_left  = powf(10, -balance_db/20);   // GraphicEqSet.cpp:52
if (balance_db < 0) balance_right = powf(10,  balance_db/20);   // GraphicEqSet.cpp:56
```
(`dsp/ptutil/DspUtil/GraphicEq/GraphicEqSet.cpp:38-59`)

* **Units: dB of attenuation on the opposite channel.** Positive = pan right
  (attenuate left); negative = pan left. It never boosts.
* GUI: rounded to integer dB (`FxController.cpp:1803`), command-line guard
  `−20 .. +20` (`FxController.cpp:317`), default 0.0
  (`fxsound/Source/GUI/FxController.h:49`). The balance slider lives in
  `fxsound/Source/GUI/FxBalanceSlider.cpp:53`.
* **Stereo only.** The mono branch of `sosProcessBuffer` ignores balance entirely
  (`SosProcess.cpp:583`). Surround (`sosProcessSurroundBuffer`) likewise.

### 8.3 `setNormalization(float gain_db)`

```cpp
normalization_gain_cache = gain_db;
sosSetNormalization(sos, powf(10, gain_db/20));    // stored as sos.target_rms
```
(`GraphicEqSet.cpp:62-74` → `dsp/ptutil/SOS/SosSet.cpp:267-279`)

The **dB value is converted to a linear target RMS**, not to a gain. Per buffer
(`SosProcess.cpp:677-723`), stereo only, and only if `target_rms != 1.0f`:

```
current_rms  = sqrt(sum_squares / (frames*2))        ; sum over post-master-gain, post-balance samples
if current_rms < 1e-6 : current_rms = 1e-6
target_gain  = target_rms / current_rms
target_gain  = clamp(target_gain, 0.01, 1.0)          ; −40 dB .. 0 dB, attenuate-only
gain_diff    = |target_gain − normalization_gain|
if target_gain > normalization_gain:  alpha = 0.0005 + gain_diff*0.001   ; slow attack
else:                                 alpha = 1.0                        ; instant release
alpha = min(alpha, 0.5)
normalization_gain += (target_gain − normalization_gain) * alpha
apply normalization_gain to the whole buffer (no ramp)
```
Constants at `SosProcess.cpp:688-694` and `:699-708`.

* `target_rms == 1.0` (i.e. `gain_db == 0.0`) **disables** the stage — this is the
  default (`Sos.cpp:46`).
* The "slow attack" branch yields α ≈ 0.0005 ⇒ a time constant of ~2000 buffers. At
  480-frame buffers / 48 kHz that is ~20 s. The release is instantaneous (α = 1).
* **No public GUI control exposes this** — `setNormalization` is never called from
  `fxsound/Source/GUI/`. It is dead in the shipping app but present in the API.

### 8.4 `setVolumeLeveling(float gain_db)` — the parameter is **not** dB

```cpp
constexpr float kVolumeLevelingMaxControlValue = 4.0f;   // GraphicEqSet.cpp:34
constexpr float kVolumeLevelingMaxTargetRms    = 0.5f;   // GraphicEqSet.cpp:35
control = clamp(gain_db, 0.0f, 4.0f);                    // GraphicEqSet.cpp:83
target_rms = (control / 4.0f) * 0.5f;                    // GraphicEqSet.cpp:86-87
sosSetVolumeLeveling(sos, target_rms);
```
(`GraphicEqSet.cpp:76-89`)

* **Units: an abstract 0..4 "amount" slider.** The parameter name `gain_db` is wrong.
* GUI range `0 .. 4`, step **0.5** (`fxsound/Source/GUI/FxAudioControls.cpp:330`);
  the controller rounds to the nearest 0.5 (`FxController.cpp:1791`); command-line guard
  `if (vl < 0 || vl > 4)` (`FxController.cpp:304`). Default `0.0`
  (`fxsound/Source/GUI/FxController.h:48`).
* `target_rms == 0` **disables** the stage and resets its whole state machine
  (`SosSet.cpp:292-320`).

The leveler itself, `applyVolumeLeveling` (`dsp/ptutil/SOS/SosProcess.cpp:139-472`), is
the most elaborate piece of DSP in the tree. Full constant table
(`SosProcess.cpp:38-76`):

| Constant | Value | Meaning |
|---|---|---|
| `kVolumeLevelingCeiling` | 1.0 | hard clip ceiling |
| `kVolumeLevelingAttackAlpha` | 0.10 | gain-down smoothing per buffer |
| `kVolumeLevelingReleaseAlphaFast` | 0.05 | gain-up, small gap |
| `kVolumeLevelingReleaseAlphaSlow` | 0.02 | gain-up, large gap |
| `kVolumeLevelingReleaseGapThreshold` | 0.15 | ratio that selects slow vs fast |
| `kVolumeLevelingPredictionStrength` | 0.35 | RMS gradient extrapolation |
| `kVolumeLevelingPredictionClamp` | 0.15 | ±15 % clamp on the prediction |
| `kVolumeLevelingPredictionMissRatio` | 0.35 | mis-prediction detector |
| `kVolumeLevelingSidechainHpfHz` | 120.0 | detector high-pass |
| `kVolumeLevelingToneLowHz` | 180.0 | tonality band split |
| `kVolumeLevelingToneBodyHz` | 1200.0 | ” |
| `kVolumeLevelingTonePresenceHz` | 4500.0 | ” |
| `kVolumeLevelingMinRatioPerBuffer` | 0.8912509381337456 | = 10^(−1 dB/20); max gain drop per buffer |
| `kVolumeLevelingTonalityDbRange` | 7.0 | dB → score normaliser |
| `kVolumeLevelingTonalitySmoothing` | 0.08 | tonality score LP |
| `kVolumeLevelingMuffledTargetBoost` | 0.12 | +12 % target when muffled |
| `kVolumeLevelingClearTargetReduction` | 0.18 | −18 % target when bright |
| `kVolumeLevelingClearCeilingReduction` | 0.08 | ceiling trim when bright |
| `kVolumeLevelingHeadroomTimeSeconds` | 60.0 | headroom-score time constant |
| `kVolumeLevelingHeadroomTargetBoost` | 0.08 | |
| `kVolumeLevelingHeadroomTargetReduction` | 0.14 | |
| `kVolumeLevelingHeadroomComfortThreshold` | 0.18 | |
| `kVolumeLevelingHeadroomNearCeilingThreshold` | 0.985 | |
| `kVolumeLevelingHeadroomHitThreshold` | 0.002 | ceiling-hit ratio |
| `kVolumeLevelingVeryQuietRmsThreshold` | 0.035 | |
| `kVolumeLevelingQuietAudiblePeakThreshold` | 0.0035 | |
| `kVolumeLevelingQuietFullBoostPeak` | 0.02 | |
| `kVolumeLevelingQuietMaxGain` | 10.0 | +20 dB ceiling on quiet boost |
| `kVolumeLevelingQuietReleaseAlpha` | 0.18 | |
| `kVolumeLevelingQuietActivationSeconds` | 10.0 | must stay quiet this long |
| `kVolumeLevelingQuietActivationRampSeconds` | 2.0 | |
| `kVolumeLevelingQuietFloorReleaseRmsThreshold` | 0.06 | |
| `kVolumeLevelingQuietFloorReleaseAlpha` | 0.02 | |
| `kVolumeLevelingQuietFloorSilenceDecayAlpha` | 0.08 | |
| `kVolumeLevelingQuietPeakBucketSeconds` | 1.0 | |
| `kVolumeLevelingQuietPeakTargetRatio` | 0.98 | |
| `kVolumeLevelingQuietPeakFloorRaiseTimeSeconds` | 6.0 | |
| `SOS_VOLUME_LEVELING_HISTORY_SIZE` | 6 | RMS power ring (`u_sos.h:24`) |
| `SOS_VOLUME_LEVELING_PEAK_WINDOW_SIZE` | 30 | 30 × 1 s peak buckets (`u_sos.h:25`) |

Algorithm sketch (`SosProcess.cpp:139-472`):
1. Per-channel one-pole HPF sidechain at 120 Hz; accumulate `sum_squares` and `peak`
   from the **filtered** signal (`:174-180`).
2. Three cascaded one-pole LPFs per channel (180 / 1200 / 4500 Hz) split the *unfiltered*
   signal into low/body/presence/air energies (`:186-199`).
3. `tonality_db = 10·log10((presence + 0.75·air) / (1.15·body + 0.85·low))`, normalised by
   7 dB and smoothed at α = 0.08 (`:213-219`).
4. `effective_target_rms` and `effective_ceiling` are modulated by the tonality and a
   60-s "headroom score" (`:234-243`).
5. Averaged RMS over a 6-buffer power ring, plus a gradient prediction with a
   mis-prediction detector (`:246-305`).
6. `desired_gain = effective_target_rms / predicted_rms`, capped at
   `max_gain_cap = max(effective_target_rms/0.125, quiet_gain_floor)` (`:244`), floored at
   `quiet_gain_floor`.
7. Gain is **linearly ramped across the buffer** from `gain_start` to `gain_end`
   (`SosProcess.cpp:376-379`) — so it is sample-accurate within the buffer.
8. Post-gain, samples are hard-clipped to `±effective_ceiling` (`:693-697`).
9. A 30-second rolling window of 1-second peak buckets drives a "quiet gain floor" that
   can reach ×10 (`:424-447`).

`sqrtf`, `log10`, `fabs`, `fmax`, `fmin` are called per buffer; the per-sample loops are
multiply/add only. This is RT-safe (no allocation, no locks), but expensive.

### 8.5 `setFilterQ(float q_multiplier)`

```cpp
cast_handle->Q_multiplier = q_multiplier;      // GraphicEqSet.cpp:113
GraphicEq_InitSections(hp_GraphicEq);          // GraphicEqSet.cpp:115 — FULL REBUILD
```
* **Units: dimensionless multiplier on the derived per-band Q.**
* GUI range `1 .. 3`, step **0.5** (`fxsound/Source/GUI/FxAudioControls.cpp:348`),
  rounded to 0.5 (`FxController.cpp:1827`), command-line guard `fq < 1 || fq > 3`
  (`FxController.cpp:328`). Default `1.0` (`fxsound/Source/GUI/FxController.h:50`).
* `GraphicEq_InitSections` (`GraphicEqInitSections.cpp:32-54`) **resets
  `sampling_freq` to 44100** and rebuilds every band frequency and Q. The real sample
  rate is restored on the next `GraphicEqProcess` call, which detects the mismatch and
  runs `GraphicEqReCalcAllBandCoeffs` (`GraphicEqProcess.cpp:48-54`) — *on the audio
  thread*. See §12.

Base Q derivation (`GraphicEqSet.cpp:515-524`):
```
r = (max_band_freq / min_band_freq) ^ (1 / (num_bands − 1))
Q = sqrt(r) / (r − 1)
Q *= Q_multiplier
if (Q < 1.0) Q = 1.0
```
For 31 bands, 20 Hz..20 kHz: `r = 1000^(1/30) = 1.25893`, `Q = 1.12202/0.25893 = 4.3334`
— matching the comment `ISO 31 Bands (Q = 4.33336544)` at `GraphicEqSet.cpp:421`.
Other documented values: 15 bands → 2.14757848 (`:410`), 20 bands → 2.82774258 (`:415`),
5 bands → 1 (`:400`).

A further per-section Q limiter runs inside `filtCalcParametric`
(`dsp/ptutil/Filt/FiltCalcBiqd.cpp:146-176`):
```
FILT_Q_UPPER_LIMIT_FREQ 60.0   FILT_Q_LOWER_LIMIT_FREQ 20.0
FILT_Q_UPPER_LIMIT      20.0   FILT_Q_LOWER_LIMIT       1.0
if f_c < 60 Hz : Q ≤ (f_c − 20)·(19/40) + 1
FILT_BOOST_WARP_LEVEL   6.0    FILT_BOOST_MAX_Q 20.0   FILT_BOOST_MIN_Q 0.2
if |boost| < 6 dB : Q ≤ |boost|·(19.8/6) + 0.2
```

### 8.6 Band layout

`GraphicEqReSetAllBandFreqs` (`GraphicEqSet.cpp:362-528`) uses **hard-coded tables** for
the five supported counts, and geometric spacing otherwise (`:495-509`):

| Bands | min/max Hz | Centre frequencies | Cite |
|---|---|---|---|
| 5 | 62.5 / 16000 | 62.5, 250, 1000, 4000, 16000 | `GraphicEqSet.cpp:430-434` |
| 10 | 62.5 / 16000 | 62.5, 115.734, 214.311, 396.85, 734.867, 1360.79, 2519.84, 4666.12, 8640.48, 16000 — **legacy geometric grid, not ISO** | `GraphicEqSet.cpp:441-449` |
| 15 | 25 / 16000 | 25, 40, 63, 100, 160, 250, 400, 630, 1000, 1600, 2500, 4000, 6300, 10000, 16000 | `GraphicEqSet.cpp:456-461` |
| 20 | 20 / 16000 | 20, 31.5, 40, 63, 80, 125, 160, 250, 315, 500, 630, 1000, 1250, 2000, 2500, 4000, 5000, 8000, 10000, 16000 | `GraphicEqSet.cpp:468-473` |
| 31 | 20 / 20000 | 20, 25, 31.5, 40, 50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600, 2000, 2500, 3150, 4000, 5000, 6300, 8000, 10000, 12500, 16000, 20000 | `GraphicEqSet.cpp:480-486` |

Band index in the *public* API is **0-based**; every private call adds 1
(`DfxDspEq.cpp:463`, `:476`, `:488`, `:499`, `:508`). Internal band numbers are 1-based.

`getEqBandFrequencyRange(band, &lo, &hi)` (`GraphicEqGet.cpp:105-168`) returns the
half-octave edges around each centre:
```
ratio = max_band_freq / min_band_freq
band 1        : lo = min_band_freq                                (pinned)
band n (n>1)  : lo = round(min_band_freq · ratio^((2n−3)/(2N−2))), then +1 if <1000 else +10
band N        : hi = max_band_freq                                (pinned)
band n (n<N)  : hi = round(min_band_freq · ratio^((2n−1)/(2N−2)))
num_bands == 1: lo = hi = the band's own frequency, and Q is forced to 1.0
```

`setNumBands` (`GraphicEqSet.cpp:118-247`) rejects `<1` or `>31` (`:128`), no-ops if
unchanged (`:132`), **mutates the process-global `DFXP_GRAPHIC_EQ_NUM_BANDS`**
(`GraphicEqSet.cpp:154`, declared `extern int` at `dsp/ptutil/include/dfxpDefs.h:263`,
defined `= 31` at `dsp/DfxDspEq.cpp:32`), then remaps the old per-band gains onto the new
layout by *relative position* (linear interpolation when growing, equidistant selection
when shrinking, `:204-245`).

### 8.7 Per-band boost/cut

`setEqBandBoostCut(band0, dB)` → `dfxpEqSetBandBoostCut(handle, STORAGE_ALL, band0+1, dB)`
(`dsp/DfxDspEq.cpp:504-509`):
* Clamped to **±12 dB** (`DFXP_GRAPHIC_EQ_MIN/MAX_BOOST_OR_CUT_DB` =
  `dsp/ptutil/include/dfxpDefs.h:264-265`; applied at `dsp/ptutil/dfxp/dfxpEq.cpp:245-248`).
* Written to memory via `GraphicEqSetBandBoostCut` **and** to the registry as `"%.2f"`
  (`dfxpEq.cpp:274`).
* `GraphicEqSetBandBoostCut` (`GraphicEqSet.cpp:258-313`) additionally:
  * sets the section to unity/off when `boost == 0.0` **or** when
    `band_freq*2 >= sampling_freq` (Nyquist guard) (`:289-295`);
  * clamps again at **±20 dB** (`GRAPHIC_EQ_DEFAULT_MAX_BOOST_OR_CUT`,
    `dsp/ptutil/include/GraphicEq.h:46`; applied `GraphicEqSet.cpp:298-302`);
  * only recomputes coefficients when the value actually changed (`:305-310`).
* Sets `update_from_registry_ = true` (`DfxDspEq.cpp:506`) which arms the registry
  re-read on the **next audio buffer**.

### 8.8 Parametric section design

`filtSosParametric` (`dsp/ptutil/Filt/FiltbiqdSos.cpp:50`) → `filtCalcParametric`
(`dsp/ptutil/Filt/FiltCalcBiqd.cpp:109-222`). Classic AES conformal-mapping design:

```
center_freq = f_c / f_s                       ; normalised, f_s = 1
bandwidth   = center_freq / Q
a   = tan(π·(center_freq − 0.25))             ; warp factor      FiltCalcBiqd.cpp:181
asq = a·a
A   = 10^(boost/20)
F   = sqrt(A)                if |boost| < 6 dB
    = A/sqrt(2)              if A > 1
    = A·sqrt(2)              otherwise                            :184-188
xfmbw  = filtBW2ANGLE(a, bandwidth)
C      = 1 / tan(2π·xfmbw)
alphad = sqrt(C²·(F²−1)/(A²−F²))     (or C if |A²−F²| ≤ SPN)
alphan = A·alphad
b0 = (1+asq) + alphan·(1−asq)
b1 = 4a
b2 = (1+asq) − alphan·(1−asq)
a0 = (1+asq) + alphad·(1−asq)
a2 = (1+asq) − alphad·(1−asq)
normalise by 1/a0;  a1 := b1                  ; symmetry exploited by the run-time form
```
When `boost == 0.0` the section is turned **off** and set to `b0=1, b1=b2=a1=a2=0`
(`FiltCalcBiqd.cpp:131-137`).

Run-time form — a transposed Direct-Form II specialised for `b1 == a1`
(`SosProcess.cpp:575-580` mono, `:616-626` stereo):
```
out   = state1 + b0·in + 1.0e-30              ; SOS_FLOAT_BIAS, u_sos.h:22
state1 = (in − out)·b1 + state2
state2 = b2·in − a2·out
```
Only 2 state words per section per channel (`state1/state2` left, `state3/state4` right).

Optional DC blocker, guarded by `SOS_DO_DC_BLOCKING` (α = 0.999, `u_sos.h:23`;
`SosProcess.cpp:556-562` / `:589-599`).

---

## 9. The five effects — parameter mapping and algorithms

### 9.1 `setEffectValue` / `getEffectValue`

```cpp
void DfxDspPrivate::setEffectValue(DfxDsp::Effect e, float value) {  // DfxDspPrivate.cpp:254
    // per-effect: pick button id + knob id, cache value/10 in the section struct
    fidelity_.value = value / 10.0;                                  // :264 (etc. :270,:276,:282,:288)
    dfxpSetButtonValue(handle, button, value != 0.0 ? 1 : 0);         // :295-302
    dfxpSetKnobValue(handle, knob, value / 10.0, false);              // :304
}
```

| Effect | Button id | Knob id | Registry knob key | Registry bypass key | Default MIDI |
|---|---|---|---|---|---|
| Fidelity | `DFX_UI_BUTTON_FIDELITY` = 20 | `DFX_UI_KNOB_FIDELITY` = 1 | `valFidelity` | `byFidelity` | 51 |
| Ambience | 21 | 4 | `valAmbience` | `byAmbience` | 51 |
| Surround | 22 | 2 | `valSurround` | `bySurround` | 26 |
| DynamicBoost | 23 | 5 | `valDynamicBoost` | `byDynamicBoost` | 51 |
| Bass | 24 | 6 | `valBassBoost` | `byBassBoost` | 68 |

Ids: `dsp/ptutil/include/DfxSdk.h:40-57`. Keys: `dsp/ptutil/include/dfxpDefs.h:52-66`.
Defaults: `dsp/ptutil/include/dfxpDefs.h:204-208`.
Vocal reduction is hard-wired: knob id 7, MIDI value 5 (`u_dfxp.h:79`), button always
`IS_FALSE` (`dsp/ptutil/dfxp/dfxpGet.cpp:155-159`).

Value pipeline: `0..10` (public) → `/10` → `0.0..1.0` → `qntRToICalc` linear with 128
levels → `0..127` MIDI (`dsp/ptutil/dfxp/dfxpSet.cpp:62`,
`dsp/ptutil/dfxp/dfxpQnt.cpp:114-119`) → registry write + per-effect `Communicate*`
(`dfxpSet.cpp:97-139`).

**`setEffectValue(e, 0.0)` switches the effect's button OFF**, not merely to zero
amplitude (`DfxDspPrivate.cpp:295-302`) — i.e. 0 is a bypass, not a knob position.

`getEffectValue` reads only the **local cache** `fidelity_.value` etc.
(`DfxDspPrivate.cpp:231-252`), which is populated by `setEffectValue` and `loadPreset`
but never from the registry. See §14.2.

Music mode is fixed to `DFX_UI_MUSIC_MODE_MUSIC2` (= 2) in FxSound 13
(`dsp/DfxDspPreset.cpp:242`, registry default `dfxpGet.cpp:168`), which enables the
ambience warp factor below.

### 9.2 Fidelity → Aural Activator

Mapping (`dsp/ptutil/dfxp/dfxpComm.cpp:518-565`, qnt `dfxpQnt.cpp:132-139`):
```
midi 0..127  --linear-->  aural_drive  0.0 .. DSP_AURAL_DRIVE_MAX_VALUE · 0.8
DSP_AURAL_DRIVE_MAX_VALUE = TWO_PI/4 · 1.8 · 2.0 · 0.75 = 4.2411501   (c_aural.h:70)
PLY_FIDELITY_INTENSITY_MAX_SCALE = 0.8                                (c_play.h:81)
⇒ aural_drive ∈ [0.0, 3.39292]
Speech mode only: midi *= 1.6, clamped to 127                         (dfxpComm.cpp:537-542)
```
Sent to front/rear/side/center; **never to the subwoofer** (`dfxpComm.cpp:550-562`,
and `DSP_PLAY_ACTIVATOR_ON` is forced 0 on the sub at `dfxpComm.cpp:440`).

Algorithm (`dsp/ptechDsp/Aural/Aural032/Auralp32.c:210-300`):
```
filtH = 2nd-order Butterworth HIGH-PASS of the input        ; Auralp32.c:235-241
        state: out_minus1, out_minus2, in_minus1, in_minus2 (per channel)
        coeffs gain/a1/a0 from a fixed "tune" MIDI of 53    ; c_play.h:60
        cutoff swept 500 Hz .. 10000 Hz over MIDI 0..127    ; u_dfxp.h:58-59
filtH *= aural_drive
odd  = sin(filtH)                                          ; odd harmonics
even = max(filtH, 0)                                       ; even harmonics (half-wave)
out  = in + (aural_even·even + aural_odd·odd)              ; Auralp32.c:257
       aural_odd  = 1.5, aural_even = 0.0 (fixed)          ; Play32.c:269-270
wet/dry mix: dry 0.622047, wet 0.377953                    ; Play32.c:266-267
```

### 9.3 Surround → Widener

Mapping (`dfxpComm.cpp:786-838`, qnt `dfxpQnt.cpp:164-172`):
```
midi 0..127 --linear--> intensity 0.0 .. (1.0 · 0.7)
DSP_WID_INTENSITY_MAX_VALUE = 1.0        (c_wid.h:30)
PLY_WIDENER_BOOST_MAX_SCALE = 0.7        (c_play.h:93)
⇒ intensity ∈ [0.0, 0.7]
```
Sent to front/rear/side only; forced OFF on center and subwoofer
(`dfxpComm.cpp:450-453`).

Algorithm — **rewritten by the Theremino contributor, no longer a delay-based widener**
(`dsp/ptechDsp/wide/Wide32/Wide32.c:212-245`):
```
gainSide = 1 + 3.0·intensity          ; 1.0 .. 3.1        Wide32.c:214
gainComp = 1 − 0.3·intensity          ; 1.0 .. 0.79       Wide32.c:215
mono = (L + R) · 0.5
outL = mono·gainComp + gainSide·(L − mono)
outR = mono·gainComp + gainSide·(R − mono)
mono input: R := 0, outL *= 0.5, outR *= 0.5              Wide32.c:239-244
```
**This is stateless.** The `dispersion_l = 169`, `dispersion_r = 218`, `width`,
`center_gain`, `center_depth` and the widener's own filter coefficients
(`Play32.c:344-356`) are still initialised and communicated but are now **dead**.

### 9.4 Dynamic Boost → Maximizer (never bypassed)

Mapping (`dfxpComm.cpp:686-784`):
```
midi = knob MIDI (default 51)
music mode 2:  midi *= 1.8    (DFXP_MUSIC_MODE2_DYNAMIC_BOOST_FACTOR, dfxpDefs.h:129)
speech mode :  midi *= 1.8    (DFXP_SPEECH_MODE_DYNAMIC_BOOST_FACTOR, dfxpDefs.h:130)
clamp midi ≤ 127                                                  dfxpComm.cpp:720-721
if bypass_dynamic_boost || bypass_all: midi = 0                   dfxpComm.cpp:732-734
midi = (int)(midi · 0.7)      (PLY_OPTIMIZER_BOOST_MAX_SCALE, c_play.h:90)  :742
gain_boost = qnt[midi]        QNT_RESPONSE_MAXI_BOOST_DSP: a piecewise dB
                              curve 0..30 dB (0.1 dB steps to 6 dB, then 0.2 dB …)
                              converted to LINEAR by 10^(dB/20)   Qntitor.cpp:511-517
max_delay  = (int)(internal_sampling_freq · MAXI_LOOK_AHEAD_DELAY)
             MAXI_LOOK_AHEAD_DELAY = 0.00075 s = 750 µs           c_max.h:49  → :765
target_level = MAXIMIZE_TARGET_LEVEL_SETTING = 0.32               c_max.h:53, dfxpComm.cpp:438
```
Range endpoints: `DSP_MAXIMIZE_GAIN_BOOST_MIN_VALUE 0`,
`DSP_MAXIMIZE_GAIN_BOOST_MAX_VALUE 30.0` dB (`c_max.h:41-42`).

Algorithm (`dsp/ptechDsp/Maximizer/Maxi32/Maxi32.c:237-480`):
```
level      = level·a0 + in_L²·filt_gain       ; 1-pole LP, cutoff MAXIMIZE_LEVEL_FILT_CUTOFF = 0.1 Hz (c_max.h:57)
sqrt_level = sqrt(level)                      ; LEFT CHANNEL ONLY drives the detector  Maxi32.c:259-261
if gain_boost·sqrt_level > target_level:
    g = target_level / sqrt_level
    if g < 1.06 : g = 1.06                    ; anti-pumping floor, Maxi32.c:288-289
else:
    g = gain_boost
delay_line[ptr] = g · max_output · in ; ptr wraps at max_delay  Maxi32.c:297-302
envelope follower with ramp-to-peak, release_time_beta = 0.997776 (default) + 1e-24 bias
out = delayed / envelope · max_output  when envelope > max_output  Maxi32.c:366-370
max_output default 0.966051 (≈ −0.3 dBFS)                         Maxi32.c:91
quantisation: KERNOISE_QUANTIZE_16 + KERNOISE_DITHER_SHAPED       Play32.c:414-415
```
**Note the dither/quantise-to-16-bit stage is initialised** even though the pipeline is
32-bit float. Port only if you can confirm it is actually applied; for a float Linux
pipeline it should be dropped.

### 9.5 Ambience → Lex reverb

Mapping (`dfxpComm.cpp:571-679`, qnt `dfxpQnt.cpp:144-151`):
```
midi = ambience MIDI (default 51)
if music_mode != MUSIC1:  midi = (int)(midi · 0.34)   (DFXP_MUSIC_MODE2_AMBIENCE_FACTOR, dfxpDefs.h:128)
room_size: fixed MIDI 64 (DSP_PLAY_LEX_ROOM_SIZE_MIDI, c_play.h:61)
           → linear 0.5 .. 1.5  (DSP_LEX_ROOM_SIZE_MIN/MAX_VALUE, c_lex.h:50-51)
decay = qnt_EXP[midi]   over PLY_DECAY_MIN_VALUE 0.095 .. PLY_DECAY_MAX_VALUE 0.95  (c_play.h:100-101)
decay = pow(decay, roomsize)                                      dfxpComm.cpp:611
lat6  = clamp(decay + 0.15, 0.25, 0.5)                            dfxpComm.cpp:613-617
if midi > 40:   wet = 0.21·1.3 = 0.2730 ; dry = 0.69·1.3 = 0.8970
else:           wet = (midi − 12)·(1/28)·0.273
                dry = 0.897 + ((40 − midi)/28)·(1 − 0.897)        dfxpComm.cpp:621-631
```
Fixed reverb parameters (`Play32.c:293-317`): `lat1 0.75`, `lat3 0.625`, `lat5 0.70`,
`lat6 0.5`, `damping 0.408290`, `bandwidth 0.350110`, `roomsize 1.0`,
`modulation_freq 0.110871`, `modulation_depth 27.7795`, `decay 0.565664`, `pre_delay 1`.
Fixed control MIDIs: rolloff 89, damping 81, depth 40, rate 28 (`c_play.h:62-65`).

**Ambience bypass is knob-dependent**: `dfxp_CommAmbienceBypass`
(`dfxpComm.cpp:1662-1709`) forces bypass when
`midi <= DFXP_MIN_EFFECTIVE_MIDI_AMBIENCE (12)` (`dfxpComm.cpp:50`, `:1688`), so any
display value ≤ 0.99 is genuinely silent. Ambience is always bypassed on the subwoofer
(`dfxpComm.cpp:1706`).

The Lex reverb is a Dattorro-style tank (`dsp/ptechDsp/Lex/Lex32/Lex32.c`) using
`DSPS_SOFT_MEM_LEX_LENGTH = 2·(8192+1) + 1.5·(0.860·96000) = 140,370` floats
(`dsp/ptutil/include/c_dsps.h:92`).

### 9.6 Bass → inline parametric shelf/peak in `Play32`

Mapping (`dfxpComm.cpp:871-920`, qnt `dfxpQnt.cpp:530-538`):
```
midi 0..127 --linear in dB--> boost 0.0 .. 15.0 dB
DSP_PLY_BASSBOOST_MIN_VALUE   0.0        (c_play.h:129)
DSP_PLY_BASSBOOST_MAX_VALUE  15.0        (c_play.h:130)
DSP_PLY_BASSBOOST_CENTER_FREQ 90.0 Hz    (c_play.h:131)
DSP_PLY_BASSBOOST_Q           2.5        (c_play.h:132)
speech mode only: midi = (int)(midi · 0.25)   (DFXP_SPEECH_MODE_BASS_BOOST_FACTOR)  :889
```
The qnt handle precomputes **128 complete biquads** at init and format change
(`QntitoBoostCut.cpp:60-82`), so the audio-thread cost is a table lookup plus five
`comRealWrite`s. Coefficients go to the **front** handle and, in surround, also to the
**subwoofer** handle (`dfxpComm.cpp:900-916`).

Note the correlated constant `DFXP_DEFAULT_BAND1_DB_VAL = 5.35`
(`dsp/ptutil/include/dfxpDefs.h:211`) — *"DEFAULT DB VALUE FOR BAND1 - IT MUST MATCH
DEFAULT FOR BASS BOOST"*. With default bass MIDI 68: `68/127·15 = 8.03 dB`, so the
comment is stale; the EQ band-1 coupling code was removed (`dfxpSet.cpp:132-139` is now
an empty comment block).

Run-time (`Play32.c:695-760`), a transposed DF-II with `b1 == a1`:
```
out1 = in1_w1 + b0·(in1 + 1e-30)
in1_w1 = (in1 − out1)·b1 + in1_w2
in1_w2 = b2·in1 − a2·out1
mono input ⇒ out2 = 0.0     (deliberate; see Play32.c:186-194)
```

### 9.7 Per-channel-group effect enablement matrix

From `dfxp_CommunicateBypassSettings` (`dfxpComm.cpp:350-511`):

| | front | rear | side | center | subwoofer |
|---|---|---|---|---|---|
| master bypass | `bypass_all` | `bypass_all \| mono/stereo` | ” | ” | ” |
| Activator (Fidelity) | on | on | on | on | **always 0** (`:440`) |
| Widener (Surround) | on | on | on | **0** (`:450`) | **0** (`:452`) |
| Bass boost | on (`:458`) | **0** | **0** | **0** | on only in surround (`:466`) |
| Vocal reduction | on | 0 | 0 | 0 | 0 |
| Ambience | on | on | on | on | **always 0** (`:1706`) |
| Dynamic boost | on | on | on | on | on |
| Legacy headphone | **hard-wired 0** (`:498`) | 0 | 0 | 0 | 0 |

---

## 10. Spectrum analyser

`getSpectrumBandValues(float* out, int n)` → `dfxpSpectrumGetBandValues`
(`dsp/DfxDspPrivate.cpp:553-556`, `dsp/ptutil/dfxp/dfxpSpectrum.cpp:105-121`) →
`spectrumGetBandValues` (`dsp/ptutil/DspUtil/spectrum/spectrumGet.cpp:30-54`).

* `n` **must equal 10** (`SPECTRUM_MAX_NUM_BANDS`,
  `dsp/ptutil/include/spectrum.h:25`; `DFXP_SPECTRUM_NUM_BANDS = 10`,
  `dsp/ptutil/include/dfxpDefs.h:153`). Otherwise `spectrumGetBandValues` returns
  `NOT_OKAY` at `spectrumGet.cpp:46` and the caller's array is left untouched. FxSound
  always passes 10 (`fxsound/Source/GUI/FxController.h:45`,
  `fxsound/Source/GUI/FxController.cpp:2891`).
* Output range **0.0 .. 1.0** (`SPECTRUM_MIN/MAX_OUTPUT_VALUE`, `spectrum.h:39-40`),
  clamped at `spectrumProcess.cpp:173-174`. Linear amplitude, not dB.
* This is a plain array copy — **RT-safe and lock-free, but also unsynchronised**: the
  audio thread writes `band_values[0..9]` at `spectrumProcess.cpp:145-146` while the GUI
  thread reads them. Torn reads are possible and benign here.

Band centres (10 bands, log-spaced 56.23 Hz .. 10 kHz, `spectrumReset.cpp:101-113`):

| Band | Centre Hz | −3 dB edges Hz | `a1` | `a2` | gain denominator | warp |
|---|---|---|---|---|---|---|
| 1 | 56.23 | 42.17–74.99 | 1.9952707978 | −0.9953348411 | 4.245657595e+02 | 0.6 |
| 2 | 100 | 74.99–133.35 | 1.9915173377 | −0.9917194870 | 2.391965397e+02 | 0.6 |
| 3 | 177.83 | 133.35–237.14 | 1.9846835136 | −0.9853206989 | 1.349296378e+02 | 1.0 |
| 4 | 316.228 | 237.14–421.70 | 1.9720410075 | −0.9740444157 | 7.631087758e+01 | 1.0 |
| 5 | 562.34 | 421.70–749.89 | 1.9480305935 | −0.9543009461 | 4.334342216e+01 | 1.3 |
| 6 | 1000 | 749.89–1333.52 | 1.9006550741 | −0.9201218454 | 2.479951362e+01 | 1.3 |
| 7 | 1778.28 | 1333.52–2371.37 | 1.8025225345 | −0.8620772515 | 1.436694455e+01 | 1.3 |
| 8 | 3162.28 | 2371.37–4216.97 | 1.5891186613 | −0.7664106181 | 8.490790030e+00 | 1.3 |
| 9 | 5623.4 | 4216.97–7498.94 | 1.1149497494 | −0.6153052550 | 5.169741233e+00 | 1.5 |
| 10 | 10000 | 7498.94–13335.21 | 0.1311997923 | −0.3874425954 | 3.264452631e+00 | 1.5 |

(`dsp/ptutil/DspUtil/spectrum/spectrumReset.cpp:118-165`; warps
`SPECTRUM_BAND_n_WARP` at `spectrum.h:28-37`.)
`gain[n] = sensitivity · warp[n] / (num_channels · denominator[n])`.

**These coefficients are hard-coded for 44.1 kHz** — the design comments give the
normalised alphas at 44100 (`spectrumReset.cpp:170-...`). `spectrumReset` is called on
sample-rate change (`spectrumProcess.cpp:52-58`) but only recomputes the *rate ratio* and
the time constant; the biquad coefficients are **not** re-derived. At 48 kHz every band
centre is therefore ~8.8 % high. Fix this in the port.

Other spectrum constants:
`SPECTRUM_DEFAULT_SENSITIVITY 1.0`, range 0..10 (`spectrum.h:50-52`);
`SPECTRUM_DEFAULT_TIME_CONSTANT 10.0` ms, range 1..200 (`spectrum.h:43-47`);
`SPECTRUM_SENSITIVITY_FACTOR 4.5` (`u_spectrum.h:26`);
`SPECTRUM_MIN_BUFFER_SIZE 128`, `SPECTRUM_MAX_DELAY_SECS 5.0` (`u_spectrum.h:36`,
`spectrum.h:54`);
`DFXP_SPECTRUM_REFRESH_RATE_MSECS 40` (`dfxpDefs.h:168`);
`DFXP_SAMPLE_SETS_PER_SAVE_SPECTRUM 1024` (`dfxpDefs.h:159`);
host buffer delay is **0 ms** in FxSound (`DfxDspPrivate.cpp:71`), so the delay
compensation ring is effectively unused.

Detector per band (`spectrumProcess.cpp:156-183`):
```
input_sum = (L+R) − (L+R)[n−2]     ; shared differencer, zero when bypassed  :73-82
out = input_sum + a1·y1 + a2·y2 + 1e-5
y2 = y1 ; y1 = out ; out *= gain
squared_filtered = (1−α)·out² + α·squared_filtered
level = clamp(fast_rsqrt_approx(squared_filtered), 0, 1)     ; ~6 % error bit-hack :161-172
```
The buffer is decimated by `internal_rate_ratio` (`spectrumProcess.cpp:62`, `:64`).

---

## 11. Per-sample state that must be reset

Anything a `reset()`/`flush()` in the Rust port must zero:

| Owner | State | Cite |
|---|---|---|
| SOS section ×N ×2ch | `state1, state2` (L), `state3, state4` (R); surround: `state_1[8], state_2[8]` | `u_sos.h:36-42`; zeroed `Sos.cpp:100-...` via `sosZeroStateAllSections` |
| SOS DC blocker | `in1_old, in2_old, outDC1_old, outDC2_old` (+ `*_oldSS[8]`) | `u_sos.h:91-95`; zeroed `SosSet.cpp:152-155` |
| SOS volume leveling | `volume_leveling_gain`, `power_history[6]`, `power_sum`, `power_index`, `power_count`, `previous_average_rms`, `previous_predicted_rms`, `alpha_sample_rate`, `sc_hpf_alpha`, `tone_*_alpha`, `sc_prev_in[8]`, `sc_prev_out[8]`, `tone_lp_state[8][3]`, `tonality_score`, `headroom_score`, `quiet_duration_seconds`, `quiet_gain_floor`, `quiet_peak_history[30]`, `quiet_peak_bucket_max`, `quiet_peak_bucket_seconds`, `quiet_peak_history_index/count` | `u_sos.h:61-85`; reset block `SosSet.cpp:292-320` |
| SOS normalization | `normalization_gain` (→ 1.0) | `Sos.cpp:47` |
| Play32 | `delay_lines[head_delay]`, `delay_line_index` | `Play32.c:206-219` |
| Play32 bass biquad | `in1_w1, in1_w2, in2_w1, in2_w2` | `Play32.c:222-226` |
| Play32 legacy filters | `in{1,2}_w{1..4}_lp`, `out{1,2}_w{1..4}_lp`, `out{1,2}_w{1..4}_hp` | `Play32.c:228-252` |
| Aural | `out1_minus1/2`, `in1_minus1/2`, and the `_2` twins | `Auralp32.c:235-241`, `:263-270` |
| Lex | whole reverb tank (`DSPS_SOFT_MEM_LEX_LENGTH` floats) | `c_dsps.h:92` |
| Maximizer | `level`, `env_l`, `env_r`, delay ring `max_delay` samples, `ptr_l`, `ptr_r`, `ramp_count_l/r` | `Maxi32.c:259`, `:297-302`, `:340` |
| Play32 vocal-reduction filters (dead) | static file-scope `xv_bs1/2`, `yv_bs1/2`, `xv_bp1/2`, `yv_bp1/2` — **`static`, i.e. shared across all five `com` handles** | `Play32.c:71-74`, cleared at `:278-288` |
| Spectrum | `in_1`, `in_2`, `sFilt[10].{out,y1,y2,level,squared_filtered}`, `band_values[10]`, `band_buf[...]`, `buffer_index`, `time_since_last_buffer_store` | `spectrumReset.cpp:89-99`, `spectrum_ResetFilter` `spectrumProcess.cpp:195-202` |
| dfxp | `universal.last_called_*` (forces re-prepare), `hash_queue_*`, `spectrum.sample_sets_since_last_spectrum_save`, `ul_total_msecs_audio_processed_time` | `u_dfxp.h:181-193`, `:288` |

The only path that performs a full reset at run time is
`dfxpClearPreviousBufferedAudio` → `dfxpBeginProcess`
(`dsp/ptutil/dfxp/dfxpProcessClear.cpp:95`), and it is **disabled** in the shipping
configuration (§6.4). So in practice FxSound never flushes reverb tails between tracks.

---

## 12. Allocation, locking and syscall audit — the RT-safety problem

**Called from `processAudio` on the time-critical audio thread:**

| Operation | Count per buffer | Cite |
|---|---|---|
| `RegOpenKeyEx`+`RegQueryValueEx` for `byAll` (master bypass) | 1 | `dfxpProcessReal.cpp:126` → `dfxpGet.cpp:231` → `dfxpSession.cpp:126` |
| Registry read for `dfxTunedTrackPlaying` | 1 | `dfxpProcessReal.cpp:136` → `dfxpGet.cpp` |
| Registry read for `byAll` **again** (processed-time accounting) | 1 | `dfxpUniversal.cpp:355` |
| Registry read for EQ `on` × 2 + 31 band reads | 33, **whenever `update_from_registry_` is armed** | `DfxDspEq.cpp:61-104` |
| `swprintf` into 1 KB stack buffers per registry access | ~2 per access | `dfxpSession.cpp:118-124` |
| `dfxpCommunicateAll` if EQ changed → ~12 more registry reads + hundreds of `comRealWrite` | occasional | `DfxDspPrivate.cpp:177` |
| `GraphicEqReCalcAllBandCoeffs` if `sampling_freq` mismatch — **31 biquad designs with `pow`, `tan`, `sqrt`** | on first buffer after any `setFilterQ`/`setNumBands` | `GraphicEqProcess.cpp:48-54` |
| `sqrtf`, `log10`, `powf`-free but transcendental-heavy volume leveling | 1 buffer's worth | `SosProcess.cpp:139-472` |
| `sin()` per sample in the Aural activator | `frames` | `Auralp32.c:246`, `:275` |
| `sqrt()` per sample in the Maximizer | `frames` | `Maxi32.c:261` |

**Allocations reachable from the audio thread:** only via `setSignalFormat` → 
`dfxpBeginProcess` → `dfxp_InitDynamicQnts` (`calloc`/`free` of ~15 tables) and
`dfxp_ComLoadAndRun` → `comSftwrAllocDspMem` (`malloc`/`realloc`,
`dsp/ptComSftDfx/Comsftwr.c:846`, `:863`). Because `setSignalFormat` is called per
buffer from the audio thread, **a format change allocates and frees memory on the
time-critical thread**. This is the single worst RT violation in the codebase.

**Locking:** there are no mutexes, critical sections or atomics anywhere in the DSP
layer. All cross-thread coordination is (a) the Windows registry and (b) unguarded
plain reads/writes of `dfxpHdlType` fields. `DfxDspPrivate::being_destroyed_` is a plain
`bool` (`dsp/u_DfxDsp.h:116`) read from the audio thread and written from the GUI thread
with no synchronisation — a data race, benign in practice on x86 but not portable.

### Linux / PipeWire equivalents

| Windows mechanism | What it achieves | Linux replacement |
|---|---|---|
| Registry `HKCU\SOFTWARE\DFX\13\23\LASTUSED_DFXP\*` as the GUI↔DSP bus | cross-process parameter transport + persistence | **Split the two.** Persistence → a TOML/JSON file under `$XDG_CONFIG_HOME/fxsound/`, written debounced from the UI thread. Transport → an in-process lock-free channel (`triple_buffer` for a whole-parameter snapshot, or `rtrb`/`ringbuf` SPSC for deltas). **Never touch the filesystem from `process()`.** |
| WASAPI loopback capture + render on a dedicated `THREAD_PRIORITY_TIME_CRITICAL` thread | system-wide interception | A **PipeWire filter node** (`pw_filter`, `libspa` `SPA_AUDIO_FORMAT_F32`) inserted as the default sink via a `null-sink` + `loopback`, or an in-process `pipewire-rs` `Stream` in `PW_DIRECTION_INPUT|OUTPUT` duplex. PipeWire already runs your `process` callback on an RT-scheduled thread with `RLIMIT_RTPRIO`; do not spawn your own. |
| `dfxSharedUtil` shared memory for spectrum + processed time | GUI reads meters from another process | Not needed in a single-process Rust app: publish spectrum through a `triple_buffer::Output<[f32;10]>` or an `[AtomicU32;10]` of bit-cast floats. |
| `MessageBox(NULL, L"TTEST", …)` on init failure (`DfxDspPrivate.cpp:77`) | (debug leftover) | Delete. Return `Result::Err`. |
| `wchar_t` / `std::wstring` paths | UTF-16 Win32 paths | `std::path::PathBuf` / `&str` (UTF-8). Preset files are `.fac` (`DfxDspPreset.cpp:106`, `:127`). |

---

## 13. What the registry code stores

`dsp/DfxDspRegistry.cpp` contains exactly one function,
`DfxDspPrivate::writeRegistrySessionLongValue(long, wchar_t* key_name)`
(`dsp/DfxDspRegistry.cpp:30-52`). It writes a `long`, formatted `"%ld"`
(`DfxDspRegistry.cpp:45`), to:

```
HKEY_CURRENT_USER\SOFTWARE\<registry_product_name>\<major_version>\<vendor_code>\LASTUSED_DFXG\<key_name>
                  └─ "DFX"                └─ 13        └─ 23
```
(format string `dsp/DfxDspRegistry.cpp:37-43`; constants
`DFXG_REGISTRY_TOP_WIDE L"SOFTWARE"` `:23`, `DFXG_REGISTRY_LASTUSED_WIDE L"LASTUSED_DFXG"`
`:25`, buffer 1024 wchars `:21-22`). It no-ops when `vendor_code == 0`
(`DfxDspRegistry.cpp:35`).

**It is never called.** The only call site is commented out
(`dsp/DfxDspPreset.cpp:94-95`, *"SHOULD LET JUCE REMEMBER CURRENT PRESET SELECTION"*).
Note it writes to `LASTUSED_DFXG` (GUI hive) while everything that *is* live writes to
`LASTUSED_DFXP` (`dfxpDefs.h:40`) — two different subtrees.

### The registry keys that actually matter

Path template (`dfxpSession.cpp:118-124`, `dfxpEq.cpp:122-131`):
```
HKCU\SOFTWARE\DFX\13\23\LASTUSED_DFXP\<key>
HKCU\SOFTWARE\DFX\13\23\LASTUSED_DFXP\EQ\<key>
```

| Key | Type | Default | Written by | Read by | Cite |
|---|---|---|---|---|---|
| `valFidelity` | int MIDI 0..127 | 51 | `setEffectValue` | every buffer (via `Communicate*`) | `dfxpDefs.h:52`, `:204` |
| `valAmbience` | int MIDI | 51 | ” | ” | `dfxpDefs.h:53`, `:205` |
| `valSurround` | int MIDI | 26 | ” | ” | `dfxpDefs.h:55`, `:206` |
| `valDynamicBoost` | int MIDI | 51 | ” | ” | `dfxpDefs.h:54`, `:207` |
| `valBassBoost` | int MIDI | 68 | ” | ” | `dfxpDefs.h:56`, `:208` |
| `valVocalReduction` | int | 5, unused | — | — | `dfxpDefs.h:57`, `u_dfxp.h:79` |
| `byAll` | int 0/1 | 0 (not bypassed) | `powerOn` | **every buffer, twice** | `dfxpDefs.h:61`, `dfxpGet.cpp:177-179` |
| `byFidelity` / `byAmbience` / `bySurround` / `byDynamicBoost` / `byBassBoost` | int 0/1 (inverted sense) | 0 | `setEffectValue`, `dfxpSetButtonValue` | `dfxp_CommunicateBypassSettings` | `dfxpDefs.h:62-66`, `dfxpGet.cpp:183-214` |
| `byHeadphone` | int 0/1 | **1 = bypassed** | — | — | `dfxpGet.cpp:215-220` |
| `modeMusicMode` | int 1/2/3 | **2** (`MUSIC2`) | preset load | `Communicate*` | `dfxpDefs.h:74`, `dfxpGet.cpp:168` |
| `temporaryBypassAll` | int 0/1 | 0 | `dfxpSetTemporaryBypassAll` | — | `dfxpDefs.h:75` |
| `dfxTunedTrackPlaying` | int 0/1 | 0 | `dfxpSetDfxTunedTrackPlaying` | **every buffer** | `dfxpDefs.h:76`, `dfxpProcessReal.cpp:136` |
| `longest_buffer_msecs` | int ms | 0 | `dfxp_StoreLongestBufferSize` | (lean-and-mean: unused) | `dfxpDefs.h:95` |
| `EQ\on` | int 0/1 | **1** | `eqOn`, presets | per buffer when armed | `dfxpDefs.h:79-80`, `dfxpEq.cpp:164` |
| `EQ\Band1` … `EQ\Band31` | float `"%.2f"`, clamped ±12 | 0.00 | `setEqBandBoostCut`, presets | per buffer when armed | `dfxpDefs.h:81`, `dfxpEq.cpp:274`, `:371-374` |
| `date_last_used`, `date_installed`, `times_run`, `installed_language` | long / int | — | init | init | `dfxpDefs.h:41-43`, `:92` |
| `REGISTRATION\*`, `INSTALLATION\*` | — | — | licensing (vestigial) | — | `dfxpDefs.h:83-92` |

**Everything else the GUI persists (`master_gain`, `balance`, `filter_q`,
`volume_leveling`, `num_bands`) lives in the JUCE settings file, not the registry** —
`fxsound/Source/GUI/FxController.cpp:1792`, `:1805`, `:1817`, `:1829`, `:1781`. Those
five are pushed into the DSP on startup and never read back from it.

**Port note:** the entire registry layer exists because in the original architecture the
DSP lived inside `dsound.dll`/`winmm.dll` hooks in *other processes*. In a single-process
Rust app it collapses to plain struct fields. Keep only the **persistence** semantics
(defaults, ranges, `"%.2f"` rounding of EQ bands if you want preset-file bit-compatibility).

---

## 14. Bugs, dead code and traps found while reading

1. **`isPowerOn()` is inverted.** `powerOn(true)` sets `DFX_UI_BUTTON_BYPASS = 0`
   (`DfxDspPrivate.cpp:204`), but `isPowerOn()` returns `true` when the bypass value is
   non-zero (`DfxDspPrivate.cpp:220-228`) — i.e. it reports "power on" when the engine is
   bypassed. It is never called anywhere in the tree, which is why it survived. **Do not
   replicate; expose `is_bypassed()` or a correct `is_enabled()`.**

2. **`getEffectValue` / `setEffectValue` use different units and the getter is stale.**
   Setter takes 0..10 and stores `value/10`; getter returns the stored 0..1
   (`DfxDspPrivate.cpp:231-252`, `:254-305`). Worse, the cache is only ever written by
   `setEffectValue` and preset loading, never initialised from the registry — so a fresh
   `DfxDsp` reports all effects at 0.0 while the DSP is actually running at MIDI
   51/51/26/51/68. The GUI works around this by always pushing values down
   (`fxsound/Source/GUI/FxController.cpp:1088`). **In Rust, make the getter return exactly
   what the setter took and seed it from the persisted config.**

3. **`preset_list_handle_` is never initialised** in the constructor
   (`DfxDspPrivate.cpp:53-115` sets `dfxp_handle_`, `slout1_`, `midi_to_rval_qnt_handle_`,
   `rval_to_midi_qnt_handle_` — but not `preset_list_handle_`), yet the destructor tests
   it and calls `prelstFreeUp` (`DfxDspPrivate.cpp:124-129`). Undefined behaviour on every
   teardown.

4. **Silence detection under-scans integer buffers** — iterates `frames*nch` *bytes*
   instead of `frames*nch*bps/8` (`dfxpUniversal.cpp:294-304`).

5. **Spectrum biquads are hard-coded for 44.1 kHz** and are not recomputed on rate change
   (`spectrumReset.cpp:118-165` vs `spectrumProcess.cpp:52-58`).

6. **`setFilterQ`/`setNumBands` reset the EQ's `sampling_freq` to 44100**
   (`GraphicEqInitSections.cpp:43`), forcing a 31-biquad redesign on the next audio
   buffer (`GraphicEqProcess.cpp:48-54`).

7. **`DFXP_GRAPHIC_EQ_NUM_BANDS` is a mutable non-atomic global** (`DfxDspEq.cpp:32`,
   mutated at `GraphicEqSet.cpp:154`) read by the audio thread
   (`DfxDspEq.cpp:81`, `dfxpEq.cpp:242`).

8. **`exportPreset` has a surprising side effect**: it applies the source preset to the
   live engine before writing the copy (`DfxDspPreset.cpp:124`). Exporting changes your
   current sound.

9. **`getPresetInfo` swallows every error** — `valsRead` failure, null handle and
   `valsGetComment` failure all fall into empty `{}` blocks
   (`DfxDspPreset.cpp:372-384`), then dereferences `vals_hdl` anyway.

10. **`MessageBox(NULL, L"TTEST", L"TEST", MB_OK)` ships in the constructor's error path**
    (`DfxDspPrivate.cpp:77`).

11. **Widener legacy parameters are dead** — `dispersion_l/r`, `width`, `center_gain`,
    `center_depth` are still computed and communicated but `Wide32.c:212-245` ignores
    them.

12. **The `xv_bs*/yv_bs*/xv_bp*/yv_bp*` vocal-reduction filter states are file-scope
    `static`** (`Play32.c:71-74`) and would be shared by all five channel-group instances
    if vocal reduction were ever enabled.

13. **`DFXP_DEFAULT_BAND1_DB_VAL 5.35`** claims to track the bass-boost default but the
    coupling code was deleted (`dfxpDefs.h:210-211`, `dfxpSet.cpp:132-139`).

---

## 15. Rust port: proposed API and the real-time-safety contract

### 15.1 Types

```rust
//! crate: fxsound-dsp   (no_std-friendly core; std only for the config/persistence layer)

/// Enum order is ABI with presets and the UI slider array. DO NOT REORDER.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Effect {
    Fidelity     = 0,
    Ambience     = 1,
    Surround     = 2,
    DynamicBoost = 3,
    Bass         = 4,
}
impl Effect {
    pub const COUNT: usize = 5;
    pub const ALL: [Effect; Self::COUNT] = [
        Effect::Fidelity, Effect::Ambience, Effect::Surround,
        Effect::DynamicBoost, Effect::Bass,
    ];
}

/// 0.0 ..= 10.0, the public unit the GUI slider uses. 0.0 means "effect off".
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EffectAmount(f32);
impl EffectAmount {
    pub const MIN: f32 = 0.0;
    pub const MAX: f32 = 10.0;
    pub fn new(v: f32) -> Self { Self(v.clamp(Self::MIN, Self::MAX)) }
    pub fn get(self) -> f32 { self.0 }
    /// The engine-internal 0.0..=1.0 knob position.
    pub fn knob(self) -> f32 { self.0 / 10.0 }
    /// The legacy 0..=127 MIDI value (round-half-away-from-zero over 128 levels).
    pub fn midi(self) -> u8 { (self.knob() * 127.0 + 0.5) as u8 }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SignalFormat {
    pub sample_rate: u32,   // 16_000 ..= 192_000
    pub channels:    u8,    // 1 ..= 8
}
impl SignalFormat {
    pub const MIN_RATE: u32 = 16_000;      // u_dfxp.h:46
    pub const MAX_RATE: u32 = 192_000;     // u_dfxp.h:44
    pub const MAX_INTERNAL_RATE: u32 = 48_000; // u_dfxp.h:45
    pub const MAX_CHANNELS: u8 = 8;        // u_dfxp.h:52
    /// 1, 2 or 4 — the legacy decimation ratio. Keep the field; consider
    /// replacing the decimator itself (see §16).
    pub fn internal_ratio(&self) -> u32 {
        if self.sample_rate <= Self::MAX_INTERNAL_RATE { 1 }
        else if self.sample_rate < Self::MAX_RATE     { 2 }
        else                                          { 4 }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum DspError {
    UnsupportedFormat(SignalFormat),
    BandOutOfRange { band: usize, num_bands: usize },
    BadSpectrumLen { got: usize, want: usize },
}

pub const MAX_EQ_BANDS:    usize = 31;    // GraphicEqSet.cpp:128
pub const NUM_SPECTRUM_BANDS: usize = 10; // spectrum.h:25
pub const MAX_BLOCK_FRAMES: usize = 16_384; // DAW_MAX_BUFFER_SIZE, u_dfxp.h:38
pub const EQ_BAND_LIMIT_DB: f32 = 12.0;   // dfxpDefs.h:264-265
pub const EQ_SECTION_LIMIT_DB: f32 = 20.0;// GraphicEq.h:46
```

### 15.2 The parameter snapshot (plain data, `Copy`, sent across the lock-free channel)

```rust
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Params {
    pub bypass:            bool,           // "power off"
    pub effects:           [EffectAmount; Effect::COUNT],
    pub effect_enabled:    [bool; Effect::COUNT], // amount == 0.0 also disables

    pub eq_on:             bool,           // default true
    pub num_bands:         u8,             // 1..=31, default 31
    pub band_gain_db:      [f32; MAX_EQ_BANDS], // ±12, default 0.0
    pub band_freq_hz:      [f32; MAX_EQ_BANDS], // clamped 10..=21000
    pub filter_q_mult:     f32,            // 1.0..=3.0 step 0.5, default 1.0

    pub master_gain_db:    f32,            // -20..=+20 step 2, default 0.0
    pub balance_db:        f32,            // -20..=+20 step 1, default 0.0
    pub normalization_db:  f32,            // default 0.0 == disabled
    pub volume_leveling:   f32,            // 0.0..=4.0 step 0.5, default 0.0 == disabled
}

impl Default for Params { /* exactly the table in §2.2 / §13 */ }
```

`Params` is `Copy` and contains no heap data, so it can be shipped to the audio thread
with `triple_buffer::TripleBuffer<Params>` (wait-free, always-latest-wins) — which is the
right semantic here, since parameters are absolute values, not deltas.

### 15.3 The engine

```rust
/// Everything the audio thread owns. Constructed on a normal thread, then moved
/// into the PipeWire `process` closure.
pub struct DspEngine { /* private: fixed-capacity buffers, filter state */ }

impl DspEngine {
    /// NOT real-time safe. Allocates every internal buffer once, for the worst
    /// case (MAX_BLOCK_FRAMES × MAX_CHANNELS). Call before handing the engine to
    /// the audio thread.
    pub fn new(max_frames: usize, max_channels: usize) -> Self;

    /// NOT real-time safe. Rebuilds filter coefficients and clears state.
    /// Call from the PipeWire `param_changed` / reconfigure path, never from `process`.
    pub fn prepare(&mut self, format: SignalFormat) -> Result<(), DspError>;

    /// REAL-TIME SAFE. See the contract in §15.4.
    /// `buf` is interleaved f32, `buf.len() == frames * format.channels`.
    /// Processing is in place.
    pub fn process(&mut self, buf: &mut [f32], params: &Params);

    /// REAL-TIME SAFE. Zeroes every filter/delay/detector state listed in §11.
    pub fn reset(&mut self);

    /// REAL-TIME SAFE. Frames processed while not bypassed, as a u64 frame count.
    /// Convert to ms on the UI side: `frames * 1000 / sample_rate`.
    pub fn frames_processed(&self) -> u64;

    /// REAL-TIME SAFE. Latest analyser values, 0.0..=1.0 linear.
    pub fn spectrum(&self) -> [f32; NUM_SPECTRUM_BANDS];

    // --- Query helpers, pure functions of `Params` + format; safe anywhere. ---
    pub fn band_centre_hz(num_bands: u8, band: usize) -> Result<f32, DspError>;
    pub fn band_range_hz(num_bands: u8, band: usize) -> Result<(f32, f32), DspError>;
    pub fn band_q(num_bands: u8, q_mult: f32) -> f32;
}
```

Pairing with PipeWire:

```rust
pub struct DspHandle {          // lives on the UI thread
    tx:       triple_buffer::Input<Params>,
    spectrum: triple_buffer::Output<[f32; NUM_SPECTRUM_BANDS]>,
    frames:   Arc<AtomicU64>,
    params:   Params,           // authoritative UI-side copy, also what gets persisted
}

pub struct DspProcessor {       // moved into the PipeWire process callback
    engine:   DspEngine,
    rx:       triple_buffer::Output<Params>,
    spectrum: triple_buffer::Input<[f32; NUM_SPECTRUM_BANDS]>,
    frames:   Arc<AtomicU64>,
}

impl DspProcessor {
    #[inline]
    pub fn process(&mut self, buf: &mut [f32]) {
        let params = *self.rx.read();          // wait-free, no allocation
        self.engine.process(buf, &params);
        self.spectrum.write(self.engine.spectrum());
        self.frames.store(self.engine.frames_processed(), Ordering::Relaxed);
    }
}
```

### 15.4 Real-time-safety contract (normative)

`DspEngine::process`, `DspEngine::reset`, `DspEngine::spectrum` and
`DspEngine::frames_processed` **must**, on every path including error paths:

1. **Never allocate or free.** No `Box`, `Vec::push`, `String`, `format!`, `collect`,
   `to_owned`, `Rc`/`Arc` clone-that-may-allocate. All buffers are sized in `new()` for
   `MAX_BLOCK_FRAMES × MAX_CHANNELS`; deny the rest with
   `#![cfg_attr(feature = "rt-assert", deny(...))]` plus an
   `assert_no_alloc`-style guard in debug and CI.
2. **Never block.** No `Mutex`, `RwLock`, `Condvar`, channel `recv()`, `park`, `yield`.
   The only cross-thread primitives permitted are wait-free: `triple_buffer`,
   an SPSC ring (`rtrb`), and `Atomic*` with `Relaxed`/`Acquire`/`Release`.
3. **Never perform I/O or syscalls.** No file access, no `std::time::SystemTime`, no
   logging, no `println!`, no D-Bus, no config reads. *(This is the rule the C++ engine
   violates 35+ times per buffer via the registry — §12.)*
4. **Never panic.** No indexing that can go out of bounds, no `unwrap`, no `expect`,
   no integer overflow in debug. Slice accesses go through `get_unchecked` only behind a
   checked precondition at the top of `process`, or through chunked iterators.
5. **Be bounded in time.** Work is `O(frames × channels × active_sections)` with no
   data-dependent unbounded loops. If a reconfigure is pending, `process` copies input to
   output (passthrough) and returns — it does **not** wait.
6. **Denormals are handled explicitly.** Either set FTZ/DAZ once on the audio thread, or
   keep the original's tiny-bias trick (`+1.0e-30` per biquad, `SOS_FLOAT_BIAS`,
   `u_sos.h:22`; `+1.0e-5` in the spectrum filter, `spectrumProcess.cpp:158`;
   `+1.0e-24` in the maximizer envelope, `c_max.h:48`). Port the biases — they are part
   of the sound at very low levels.
7. **Parameter application is per buffer, with smoothing where the original smooths.**
   Volume leveling is the only stage that ramps within a buffer
   (`SosProcess.cpp:376-379`); add a short ramp on `master_gain` and `balance` too,
   because the original steps them and the GUI slider will zipper.

Non-RT-safe surface, clearly separated: `DspEngine::new`, `DspEngine::prepare`,
everything in `DspHandle`, preset load/save, and config persistence.

### 15.5 Internal module layout suggestion

```
fxsound-dsp/
  lib.rs            Effect, Params, SignalFormat, DspError, DspEngine
  eq/
    graphic.rs      band tables (§8.6), Q derivation (§8.5), num-band remap (§8.6)
    biquad.rs       filtCalcParametric port (§8.8) + TDF-II-with-b1==a1 runner
    gain.rs         master gain, balance, normalization (§8.1–8.3)
    leveling.rs     the 38-constant volume leveler (§8.4)
  effects/
    aural.rs        Fidelity  (§9.2)
    reverb.rs       Ambience  (§9.5)
    widener.rs      Surround  (§9.3)   ← ~15 lines, stateless
    bass.rs         Bass      (§9.6)
    maximizer.rs    DynamicBoost (§9.4)
  analyser/
    spectrum.rs     10-band detector (§10) — RE-DERIVE COEFFS FOR THE ACTUAL RATE
  chain.rs          the exact order in §7, channel routing matrix in §9.7
```

---

## 16. Open questions / risks for the Rust port

1. **The >48 kHz decimator is a naive sample-dropper with no anti-aliasing**
   (`Comwave.cpp:114-126` down, `:151-184` up). At 96/192 kHz it will fold everything
   above 24 kHz back into the audible band before the effects even run. *Decision
   needed:* replicate bit-for-bit (matching the Windows product's sound, warts included),
   or replace with a proper polyphase resampler / just run the effects at the native rate
   and re-derive the rate-dependent coefficients. Recommendation: **run at native rate**
   and re-derive; the only genuinely rate-dependent pieces are the Aural high-pass
   (500 Hz–10 kHz sweep, `u_dfxp.h:58-59`), the Lex tank lengths (`c_dsps.h:92`), the
   maximizer look-ahead (750 µs, `c_max.h:49`) and the bass biquad (90 Hz / Q 2.5).

2. **Spectrum coefficients are frozen at 44.1 kHz** (§10, §14.5). Re-deriving them is
   easy (they are 1-pole-pair resonators) but the *gain denominators* were computed by
   `mkfilter` and are baked in. The port must recompute both `a1/a2` and the centre-gain
   normaliser per rate, or the meters will read wrong at 48 kHz. Risk: users will notice
   the visualiser looks different from Windows.

3. **Does the maximizer's 16-bit quantise/shaped-dither stage actually run?**
   `s->num_quant_bits = KERNOISE_QUANTIZE_16` and `s->dither_type = KERNOISE_DITHER_SHAPED`
   are set (`Play32.c:414-415`) but the process loop excerpt read here
   (`Maxi32.c:237-480`) does not obviously apply them; `MAXIMIZE_QUANTIZE_ON`
   (`c_max.h:136`) is never written by `dfxpComm.cpp`. **Verify before porting** — adding
   a spurious 16-bit dither to a float pipeline would raise the noise floor by ~90 dB
   relative to a clean float path.

4. **Volume leveling is stereo-only in the mono/stereo path but the surround path passes
   `excluded_channel = 3`** (LFE) (`SosProcess.cpp:908` vs `:725`). Confirm the intended
   behaviour for 5.1/7.1 before porting, and decide what to do for PipeWire's arbitrary
   channel maps (which are *not* guaranteed to be Windows WAVE order).

5. **Channel order is assumed to be Windows WAVE order** (FL FR FC LFE BL BR [SL SR],
   `dfxpProcessReal.cpp:218-221`). PipeWire delivers a `SPA_PARAM_EnumFormat` channel
   *map*; you must translate rather than assume. Ports that ignore this will send the LFE
   through the widener.

6. **Global hotkeys.** `fxsound/Source/GUI/FxController.cpp:2880-2886` registers Win32
   `RegisterHotKey` for next/previous preset and next output device. **A Wayland client
   cannot grab global hotkeys.** Options, in order of preference: (a) register a
   `org.freedesktop.portal.GlobalShortcuts` session (available on KDE 6 / GNOME 46+, the
   user binds them in system settings); (b) expose an MPRIS2 `org.mpris.MediaPlayer2`
   interface and let the compositor's media keys drive it; (c) ship a documented
   `xdg-desktop-portal` action list plus a CLI (`fxsound --power on`, mirroring
   `FxController::applyConfig`, `fxsound/Source/GUI/FxController.cpp:344`) that the user
   binds in their own compositor config. Do **not** attempt an X11 fallback grab.

7. **Where does the filter node sit?** The Windows product is a virtual soundcard
   (kernel driver). On Linux the equivalents are, in order of preference:
   (a) a `pw_filter` node auto-inserted via a `pipewire.conf.d` fragment on the default
   sink; (b) a `null-sink` named "FxSound" plus a loopback to the real device, with the
   app moving streams; (c) a LADSPA/LV2 plugin loaded into `module-filter-chain`.
   Option (a) keeps a single process and avoids double-buffering latency; option (b) is
   the most robust against device hot-plug but adds ~1 buffer of latency and requires the
   app to manage `metadata` default-sink changes. **Unresolved.**

8. **`Params` snapshot vs. per-parameter deltas.** A whole-struct triple buffer is
   simplest and wait-free, but it means a single slider drag re-evaluates *all* EQ
   coefficients. Mitigation: keep a `dirty: u32` bitmask inside `Params` and have the
   engine recompute only the dirty groups; or precompute coefficients on the UI thread
   and ship *coefficients*, not dB values. The latter is what the C++ does for bass boost
   (128 precomputed biquads, `QntitoBoostCut.cpp:60-82`) and is the better pattern.

9. **Preset file format (`.fac`, the `vals` container)** is out of scope here but is a
   hard dependency of `loadPreset`/`savePreset`/`exportPreset`/`getPresetInfo`. Note the
   vals file version is **9.0** for DFX 12+ (`DfxDspPreset.cpp:53`), that band-count
   mismatches between preset and engine are resolved by gain interpolation while
   *keeping the live frequency table* (`DfxDspEq.cpp:168-243`), and that presets store
   MIDI 0..127 ints, not floats. Bit-compatible preset loading is a separate spec.

10. **Legal/behavioural:** the effects are AGPL-3.0 (`dsp/include/DfxDsp.h:1-19`), so a
    Rust port is fine, but any *numerical* divergence from the C++ (resampler, spectrum
    coefficients, dither) changes the product's sound. Decide early whether the goal is
    "sounds identical to FxSound on Windows" or "sounds correct"; they are not the same
    target, and items 1–3 above are where they diverge most.

11. **Unverified:** `mthConvertIntBufToRealtype` / `mthConvertRealtypeBufToIntBuf`
    (`dfxpProcessInt.cpp:93`, `:124`) were not read in full — their exact scaling
    (`/32768` vs `/32767`, and the 20-bit-in-24 handling) is undocumented here. Only
    matters if you keep the integer paths, which §4 recommends against.
