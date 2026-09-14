# 10 — The Five Audio Effects (Fidelity, Ambience, Surround, Dynamic Boost, Bass)

Reverse-engineering spec for the Rust 1.98.1 / egui-eframe 0.36 / PipeWire port of
FxSound.

Everything below comes from files read in full in
`/home/blackixxce/Загрузки/fxsound-app-main`. Every constant carries a
`path:line` citation; paths are relative to that repository root. Numeric tables
were recomputed from the cited formulas and cross-checked against the
hard-coded initialisation constants in the DSP sources — where they agree that
is stated, because the agreement is what proves the derivation correct.

Siblings: `08-dsp-api.md` (engine surface, `processAudio`, RT-safety audit),
`09-dsp-eq.md` (GraphicEq/SOS, biquad maths, the `Qnt` module). This document
does not repeat them; it covers **only** the five effects and the `ply0` DSP
program that hosts them.

---

## 0. TL;DR — effect → algorithm → file

| UI name | `DfxDsp::Effect` | Internal name | Algorithm (one line) | Live implementation |
|---|---|---|---|---|
| **Clarity** / Fidelity | `Fidelity = 0` | Aural Enhancer / "Activator" | 2nd-order Butterworth **high-pass** → drive gain → **`sin()` odd-harmonic waveshaper** → added back to dry | `dsp/ptechDsp/Aural/Aural032/Auralp32.c:208-299` |
| **Ambience** | `Ambience = 1` | "Lex" reverb | **Dattorro / Lexicon-224-style figure-of-eight plate reverb**: one-pole input LP, 4 cascaded allpass diffusers, 2 decay diffusers, 2 tapped allpasses, 4 multi-tap delays, 2 in-loop damping LPs, tapped stereo output matrix | `dsp/ptechDsp/Lex/Lex32/Lex32.c:297-683` |
| **Surround** | `Surround = 2` | "Wide" widener | **Mid/side gain widener**: `M·(1−0.3·i) + S·(1+3·i)` — rewritten 2025 by theremino; the historical dispersion-delay widener is retained but dead | `dsp/ptechDsp/wide/Wide32/Wide32.c:159-263` |
| **Dynamic Boost** | `DynamicBoost = 3` | "Maximizer" / Optimizer | **Auto-gain (≈1.59 s RMS estimator) + 0.75 ms look-ahead brick-wall peak limiter** with linear attack ramp and exponential release | `dsp/ptechDsp/Maximizer/Maxi32/Maxi32.c:237-598` |
| **Bass** | `Bass = 4` | inline in the host | Single **2nd-order parametric peaking biquad** @ 90 Hz, Q 2.5, 0…15 dB | `dsp/ptechDsp/Play/Play32/Play32.c:695-759` |

Enum (`dsp/include/DfxDsp.h:38`):

```cpp
enum Effect { Fidelity = 0, Ambience = 1, Surround = 2, DynamicBoost = 3, Bass = 4, NumEffects = 5 };
```

The five effects are **not** five independent plug-ins at runtime. They are five
parameter blocks inside one monolithic DSP program named `"ply0"`
(`dsp/ptutil/dfxp/u_dfxp.h:72`), implemented by `dspsPlayProcess32()` in
`Play32.c`. `ply0` calls four sub-algorithms in a fixed order and implements
Bass itself inline.

---

## 1. Which code is actually live

The tree contains many dead alternates (`Play32Proto.c`, `Maxi32Orig.c`,
`Maxi32Auto.c`, `Lex32org.c`, `Play32Butter.c`, …). Only files listed as
`ClCompile` in `dsp/DfxDsp.vcxproj:279-386` are compiled. The effect-relevant
entries are:

```
ptechDsp\Aural\Aural032\Auralp32.c     ptechDsp\Aural\Aural0\Auralp.c
ptechDsp\Lex\Lex32\Lex32.c             ptechDsp\Lex\Lex16\Lex16.c
ptechDsp\wide\Wide32\Wide32.c          ptechDsp\wide\Wide16\Wide16.c
ptechDsp\Maximizer\Maxi32\Maxi32.c     ptechDsp\Maximizer\Maxi16\Maxi16.c
ptechDsp\Play\Play32\Play32.c          ptechDsp\Play\Play16\Play16.c
ptechDsp\Dly32\Dly832\dly8p32.c        ptechDsp\Dly8\Dly8p.c
ptechDsp\Peq\Peq832\peq8p32.c          ptechDsp\Peq\Peq8\peq8p.c
```

Two build switches decide which half of that list is ever *executed*:

### 1.1 Bit width = 32 ⇒ the `*16.c` files are dead

`DFXP_DSP_INTERNAL_BIT_WIDTH 32` (`dsp/ptutil/dfxp/u_dfxp.h:74`) is passed to
`comSoftDspLoadAndRunNonShared()` (`dsp/ptutil/dfxp/dfxpComm.cpp:146-173`).
That reaches `comSftwrSetFunctionIndex()`, which does

```c
if(s_bit_width == 32) offset = 1;          /* dsp/ptComSftDfx/Comsftwr.c:466-468 */
...
index += offset;                            /* Comsftwr.c:804 */
```

and the function table is populated in pairs, 16-bit then 32-bit
(`Comsftwr.c:1665-1669` for `ply0`). **⇒ `dspsPlayProcess32` runs; the 16-bit
variants never do.**

### 1.2 `PT_DSP_BUILD = PT_DSP_DFX` ⇒ every `PT_DSP_DSPFX` block is compiled out

Every configuration in `dsp/DfxDsp.vcxproj:118,133,148,165,187,209` defines
`DSPSOFT_TARGET;PT_DSP_BUILD=PT_DSP_DFX`, and
`PT_DSP_DSPFX == 1`, `PT_DSP_DFX == 2` (`dsp/ptutil/include/boardrv1.h:22-23`).

Consequences you **must** replicate in Rust:

| Compiled out | Where | Consequence |
|---|---|---|
| Reverb LFO modulation / chorus | `Lex32.c:399-419, 424-426, 434-460, 547-549, 557-582` | The Ambience reverb has **no** modulation. `lat5`/`lat7` use fixed **integer** delays with no interpolation. The 8193-point cosine tables are still built (`Lex32.c:117-144`) and never read. |
| `write_meters_and_status`, `write_meter_average` | `dsp/ptutil/include/dutcom.h:161, 241` (`#define … ;`) | Metering is a no-op |
| `dutilSetClipStatus` | `dsp/ptutil/include/dutio.h:454` (`#define … ;`) | No clip flagging |
| `dutilMuteInputs` | `dsp/ptutil/include/dutio.h:228` (`#define … ;`) | No-op — `Lex32.c:321` does nothing |
| `load_parameter()` | `dsp/ptutil/include/dutcom.h:67` (`#define … ;`) | No-op |
| Demo-mode muting | `dsp/ptComSftDfx/Comsftwr.c:158-177` | Never mutes |
| Maximizer aux meter values | `Maxi32.c:605-617` | Not produced |

### 1.3 Sample format inside the DSP core

The `DSPSOFT_TARGET + PT_DSP_DFX` I/O macros are
`dsp/ptutil/include/dutio.h:173-182` (read) and `:415-425` (write):

```c
#define dutilGetInputsAndMeter(in1, in2, status)\
if( *(volatile long *)(DSP_STEREO_IN_FLAG) )\
{ in1 = ((float *)read_in_buf)[data_index];
  in2 = ((float *)read_in_buf)[data_index + 1]; }\
else { in1 = in2 = ((float *)read_in_buf)[data_index]; }

#define dutilPutOutputsAndMeter(r_val1, r_val2, status)\
if( *(volatile long *)(DSP_STEREO_IN_FLAG) )\
{ ((float *)read_out_buf)[data_index++] = r_val1;
  ((float *)read_out_buf)[data_index++] = r_val2; }\
else { r_val1 = (r_val1 + r_val2);
       ((float *)read_out_buf)[data_index++] = r_val1; }
```

So: **32-bit float, interleaved L,R, nominal ±1.0, processed in place**
(`read_in_buf == read_out_buf`, e.g. `Auralp32.c:205-206`). In mono mode the
single sample is fanned out to both `in1`/`in2` on read and the two outputs are
**summed** on write. Note `data_index` is *not* advanced on read — only on
write — so every stage must write exactly one output per input.

---

## 2. Where the effects sit — full chain with gain stages

`DfxDsp::processAudio()` → `DfxDspPrivate::processAudio()`
(`dsp/DfxDspPrivate.cpp:181-189`) → `dfxpUniversalModifySamples()` →
`dfxpModifyShortIntSamples()` (`dsp/ptutil/dfxp/dfxpProcessInt.cpp:46`) →
`dfxpModifyRealtypeSamples()` (`dsp/ptutil/dfxp/dfxpProcessReal.cpp:51`).

```
 host PCM (int8/16/20/24/32, interleaved)
    │
    │ mthConvertIntBufToRealtype()                       dfxpProcessInt.cpp:93
    ▼
 f32[] interleaved, ±1.0, in cast_handle->r_samples
    │
    ├─────────────── GRAPHIC EQ + MASTER GAIN STAGE ─────────────────┐
    │  bypass_all==0 && eq_on:  GraphicEqProcess()                   │
    │        dfxpProcessReal.cpp:152-156                             │
    │     ├ optional DC blocker            SosProcess.cpp:554-602    │
    │     ├ N cascaded biquads (per ch)    SosProcess.cpp:608-629    │
    │     ├ × master_gain                  SosProcess.cpp:630-631    │
    │     ├ × balance_left / balance_right SosProcess.cpp:630-631    │
    │     ├ normalization AGC (gain ≤ 1.0) SosProcess.cpp:678-723    │
    │     └ volume-leveling AGC (gain ≥ 1) SosProcess.cpp:725        │
    │  bypass_all==1 && eq_on:  master_gain only                     │
    │        GraphicEqProcess_MasterGainOnly → SosProcess.cpp:501-517│
    │  eq_on==0: NOTHING (not even master gain)  ← see §12 traps     │
    └────────────────────────────────────────────────────────────────┘
    │
    ├─ BinauralSyn HRIR headphone virtualisation, only if
    │  binaural_headphone_on_flag && !bypass_all && srate ≤ 48000
    │        dfxpProcessReal.cpp:176-192
    │
    ├─ (4/6/8 ch only) de-interleave into per-pair planes
    │        dfxpProcessReal.cpp:230-339
    │
    ├─ comProcessWaveBuffer()                     dfxpProcessReal.cpp:366
    │     ├ decimate by internal_rate_ratio (sample-and-hold, no filter)
    │     │      Comwave.cpp:124-146
    │     ▼
    │  ╔══════════════ ply0 == dspsPlayProcess32() ═══════════════════╗
    │  ║  Play32.c:423-878            if (s->bypass_on) → do nothing  ║
    │  ║                                                              ║
    │  ║  [vocal eliminator]   if vocal_elim_on  — ALWAYS OFF         ║
    │  ║        Play32.c:442-637 ; dfxpGet.cpp:155-159                ║
    │  ║      │                                                       ║
    │  ║      ▼                                                       ║
    │  ║  ┌──────────────┐  if activator_on                           ║
    │  ║  │ 1. AURAL     │  y = x + 0.5669295·sin(drive·HP₂(x))       ║
    │  ║  │   (Fidelity) │  Auralp32.c:235-296                        ║
    │  ║  └──────────────┘  gain stage: wet 0.377953 / dry 0.622047   ║
    │  ║      │                          (sum == 1.0 exactly)         ║
    │  ║      ▼                                                       ║
    │  ║  ┌──────────────┐  if ambience_on                            ║
    │  ║  │ 2. LEX       │  plate reverb, mono-in / stereo-out        ║
    │  ║  │   (Ambience) │  Lex32.c:297-683                           ║
    │  ║  └──────────────┘  gain stages: ×(0.6·0.5)=0.3 then          ║
    │  ║      │              wet_gain (0…0.273) / dry_gain (0.897…1)  ║
    │  ║      ▼                                                       ║
    │  ║  ┌──────────────┐  if widener_on                             ║
    │  ║  │ 3. WIDE      │  M·(1−0.3i) ± S·(1+3i)                     ║
    │  ║  │   (Surround) │  Wide32.c:214-236                          ║
    │  ║  └──────────────┘  gain stage: none (unity at i=0)           ║
    │  ║      │                                                       ║
    │  ║      ▼                                                       ║
    │  ║  ┌──────────────┐  if bassboost_on                           ║
    │  ║  │ 4. BASS      │  peaking biquad, 90 Hz / Q 2.5 / 0…15 dB   ║
    │  ║  │   (inline)   │  Play32.c:718-758                          ║
    │  ║  └──────────────┘  gain stage: b0 carries the boost          ║
    │  ║      │                                                       ║
    │  ║  [Dly8 headphone ambience] if headphone_on — ALWAYS OFF      ║
    │  ║        Play32.c:763-860 ; dfxpComm.cpp:498-499 writes 0      ║
    │  ║      │                                                       ║
    │  ║      ▼                                                       ║
    │  ║  ┌──────────────┐  UNCONDITIONAL — never bypassed            ║
    │  ║  │ 5. MAXIMIZER │  auto-gain + 0.75 ms look-ahead limiter    ║
    │  ║  │ (DynBoost)   │  Maxi32.c:237-598                          ║
    │  ║  └──────────────┘  gain stages: ×gain_boost ×max_output      ║
    │  ║                     (0.966051) then ÷env when env>max_output ║
    │  ║                     wet 1.0 / dry 0.0                        ║
    │  ╚══════════════════════════════════════════════════════════════╝
    │     ▲ replicate per channel-pair: front / rear / side (stereo),
    │     │ center / subwoofer (mono) — 5 independent ply0 instances
    │     │      dfxpComm.cpp:146-174, dfxpProcessReal.cpp:359-470
    │     │
    │     └ interpolate back up by internal_rate_ratio (sample-and-hold)
    │            Comwave.cpp:166-…
    │
    ├─ spectrumProcess() tap (post-effect)     dfxpProcessReal.cpp:488-491
    ├─ re-interleave 4/6/8 ch                  dfxpProcessReal.cpp:514-572
    │
    │ mthConvertRealtypeBufToIntBuf()          dfxpProcessInt.cpp:124
    ▼
 host PCM out
```

**Order matters and is fixed**: Fidelity → Ambience → Surround → Bass →
Dynamic Boost. The limiter is last so it catches everything the other four add.

### 2.1 Per-channel-pair routing

`dfxp_CommunicateBypassSettings()` (`dfxpComm.cpp:350-512`) writes different
enables to the five `ply0` instances:

| Effect | front | rear | side | center | subwoofer |
|---|---|---|---|---|---|
| Master bypass (`DSP_PLAY_BYPASS_ON`) | `bypass_all` | `bypass_all \| stereo_flag` | idem | idem | idem |
| Aural (`ACTIVATOR_ON`) | ✔ | ✔ | ✔ | ✔ | **always 0** |
| Lex (`AMBIENCE_ON`) | ✔ | ✔ | ✔ | ✔ | **always 0** |
| Wide (`WIDENER_ON`) | ✔ | ✔ | ✔ | **0** | **0** |
| Bass (`BASS_BOOST_ON`) | ✔ | **0** | **0** | **0** | `on & surround_flag` |
| Vocal reduction | ✔ (but knob is forced off) | 0 | 0 | 0 | 0 |
| Maximizer | always on, all five |

For a stereo Linux target only the **front** instance matters; the others exist
for Windows multichannel endpoints. Port the front path and keep the routing
table for a future surround mode.

---

## 3. Parameter plumbing: 0…10 → 0…1 → MIDI 0…127 → DSP word

### 3.1 The public value is 0…10 going in and 0…1 coming out

```cpp
// dsp/DfxDspPrivate.cpp:254-305
void DfxDspPrivate::setEffectValue(DfxDsp::Effect effect, float value) {
    ...
    fidelity_.value = (realtype)value / (realtype)10.0;      // :264
    ...
    if (value != 0.0) dfxpSetButtonValue(dfxp_handle_, button, 1);  // :295-302
    else              dfxpSetButtonValue(dfxp_handle_, button, 0);
    dfxpSetKnobValue(dfxp_handle_, knob, (realtype)value / (realtype)10.0, false); // :304
}

// dsp/DfxDspPrivate.cpp:231-252
float DfxDspPrivate::getEffectValue(DfxDsp::Effect effect) { return fidelity_.value; }  // 0..1 !
```

The setter takes **0…10**; the getter returns **0…1**. The GUI compensates by
multiplying by 10 on every read:

* `fxsound/Source/GUI/FxController.cpp:1756-1764` — `setEffectValue` rejects
  anything outside `[0, 10]`.
* `fxsound/Source/GUI/FxController.cpp:667-671` — `getEffectValue(...) * 10.0f`
  when serialising to JSON.
* `fxsound/Source/GUI/FxAudioControls.cpp:125-128` — `setEffectValue(value*10.0)`
  when populating the sliders.
* `fxsound/Source/GUI/FxController.cpp:1087-1088` — after a preset load,
  `setEffectValue(e, getEffectValue(e) * 10)` re-pushes every value.

> **Port decision.** Do not reproduce this. Use one canonical `f32` in `0.0..=1.0`
> everywhere, and convert at the UI edge only. Document in the changelog that
> `getEffectValue` used to return a tenth of what `setEffectValue` took.

### 3.2 0…1 → MIDI

`dfxpSetKnobValue()` (`dsp/ptutil/dfxp/dfxpSet.cpp:43-70`) range-checks against
`DFX_UI_MIN_VALUE 0.0` / `DFX_UI_MAX_VALUE 1.0`
(`dsp/ptutil/include/DfxSdk.h:92-93`) and calls `qntRToICalc`:

```c
/* dsp/ptutil/Qnt/Qntrtoi.cpp:96-98 */
*ip_output = (int)((r_input - r_input_min) * r_scale + (realtype)i_output_min + 0.5);
/* r_scale = (127 - 0) / (1.0 - 0.0) = 127   (Qntrtoi.cpp:68-69) */
```

⇒ **`midi = round(v01 * 127)`**, `MIDI_MIN_VALUE 0`, `MIDI_MAX_VALUE 127`
(`dsp/ptutil/include/midi.h:47,59`).

The MIDI value is written to the registry
(`dfxpSet.cpp:99-124`) and re-read on every communicate
(`dfxp_GetKnobValue_MIDI`, `dfxpGet.cpp:69-140`).

### 3.3 Knob / button ids and defaults

| Effect | `DFX_UI_KNOB_*` | `DFX_UI_BUTTON_*` | Registry value | Default MIDI | Default UI (0…10) |
|---|---|---|---|---|---|
| Fidelity | 1 | 20 | `valFidelity` | 51 | 4.02 |
| Surround | 2 | 22 | `valSurround` | 26 | 2.05 |
| Ambience | 4 | 21 | `valAmbience` | 51 | 4.02 |
| DynamicBoost | 5 | 23 | `valDynamicBoost` | 51 | 4.02 |
| Bass | 6 | 24 | `valBassBoost` | 68 | 5.35 |

Ids: `dsp/ptutil/include/DfxSdk.h:39-58`. Registry names:
`dsp/ptutil/include/dfxpDefs.h:52-56`. Defaults:
`dsp/ptutil/include/dfxpDefs.h:204-208`, wired in `dfxpGet.cpp:100-125`.
All five buttons default to **not bypassed** (`dfxpGet.cpp:183-214`).

### 3.4 ⚠ Music mode MUSIC2 is always active — and it rescales two effects

`dfxp_GetKnobValue_MIDI`'s sibling `dfxpGetButtonValue` returns
`DFX_UI_MUSIC_MODE_MUSIC2` as the default for `DFX_UI_BUTTON_MUSIC_MODE`
(`dsp/ptutil/dfxp/dfxpGet.cpp:166-174`), and `DfxDspPreset.cpp:236-242` states:

```c
/* NOTE: As of DFX Version 13, we only allow music mode 2. */
music_mode_ = DFX_UI_MUSIC_MODE_MUSIC2;
```

No code path in the shipping app ever sets it to `MUSIC1` or `SPEECH`.
The factors (`dsp/ptutil/include/dfxpDefs.h:128-132`):

```c
#define DFXP_MUSIC_MODE2_AMBIENCE_FACTOR        0.34
#define DFXP_MUSIC_MODE2_DYNAMIC_BOOST_FACTOR   1.8
#define DFXP_SPEECH_MODE_DYNAMIC_BOOST_FACTOR   1.8
#define DFXP_SPEECH_MODE_BASS_BOOST_FACTOR      0.25
#define DFXP_SPEECH_MODE_FIDELITY_FACTOR        1.6
```

Applied at:

* Ambience — `dfxpComm.cpp:595-597`: `if (music_mode != MUSIC1) pc_liveliness = (int)(pc_liveliness * 0.34);`
* Dynamic Boost — `dfxpComm.cpp:709-721`: `pc_gain_boost = (int)(1.8 * pc_gain_boost)`, clamped to 127.
* Fidelity and Bass are untouched in MUSIC2 (only SPEECH scales them, `dfxpComm.cpp:537-542, 892-897`).

**This is not cosmetic.** Under MUSIC2:

* **Ambience is fully bypassed for UI ≤ 3.0** — `int(midi·0.34) ≤ 12` triggers
  `DFXP_MIN_EFFECTIVE_MIDI_AMBIENCE` (`dfxpComm.cpp:50, 1688-1691`). The first
  30 % of the Ambience slider does nothing at all.
* **Dynamic Boost saturates at UI ≈ 5.59** (`midi ≥ 71` → `71·1.8 = 127.8` →
  clamped to 127). The top 44 % of the Dynamic Boost slider does nothing.

A faithful port must reproduce this or deliberately fix it — see §14.

### 3.5 DSP parameter address space

Each sub-algorithm gets `2 × DSPS_MAX_NUM_PARAMS = 256` floats
(`dsp/ptutil/include/c_dsps.h:50`) in one flat array
`dsp_params[DSPFX_MAX_NUM_PROCS * 2 * DSPS_MAX_NUM_PARAMS]`
(`dsp/ptComSftDfx/u_comSftwr.h:43`), zeroed at init
(`dsp/ptComSftDfx/ComsftwrCPP.cpp:80-81`).

```c
/* dsp/ptutil/include/c_play.h:71-75 */
#define DSP_PLAY_AURAL_PARAM_OFFSET      (0 * DSPS_MAX_NUM_PARAMS * 2)   /*   0 */
#define DSP_PLAY_LEX_PARAM_OFFSET        (1 * DSPS_MAX_NUM_PARAMS * 2)   /* 256 */
#define DSP_PLAY_WIDENER_PARAM_OFFSET    (2 * DSPS_MAX_NUM_PARAMS * 2)   /* 512 */
#define DSP_PLAY_DELAY_PARAM_OFFSET      (3 * DSPS_MAX_NUM_PARAMS * 2)   /* 768 */
#define DSP_PLAY_OPTIMIZER_PARAM_OFFSET  (4 * DSPS_MAX_NUM_PARAMS * 2)   /*1024 */
```

The Play host's own parameters live *above* the Aural block's last parameter, in
the same 0…255 window (`c_play.h:179-210`), which is why `DSP_PLAY_BYPASS_ON` is
at word 36 and `DSP_PLAY_B0` (bass `b0`) at word 43.

Writes go through `comRealWrite`/`comLongIntWrite`
(`dsp/ptutil/COM/Comwrite.cpp:104-180`) → `comSftwrWriteParam`
(`Comsftwr.c:69-94`), i.e. **a plain store into a shared float array, read
lock-free by the audio thread on the next buffer**. There is no smoothing and
no atomicity — a torn parameter update is possible in principle. See
`08-dsp-api.md §12`.

> **Port decision.** Replace with a lock-free SPSC parameter snapshot (triple
> buffer or `ArcSwap<Params>`) plus per-buffer coefficient interpolation for the
> gains that are audible when stepped (Aural drive, Lex wet/dry, Wide intensity,
> Maxi gain_boost).

### 3.6 Which parameters are *never* written from the host

Verified by grep across `dsp/ptutil/dfxp/*.cpp` and `dsp/DfxDsp*.cpp`: none of
`LEX_LAT1_COEFF`, `LEX_LAT3_COEFF`, `LEX_LAT5_COEFF`, `LEX_PRE_DELAY`,
`AURAL_ODD`, `AURAL_EVEN`, `MAXIMIZE_MAX_OUTPUT`, `MAXIMIZE_QUANTIZE_ON`,
`MAXIMIZE_QUANTIZE_NUM_BITS`, `MAXIMIZE_DITHER_TYPE`, `DSP_WID_WIDTH`,
`DSP_WID_REVERSE_WIDTH`, `DSP_WID_CENTER_GAIN`, `DSP_WID_CENTER_DEPTH`
appear anywhere. They keep whatever `DSPS_PLAY_INIT` set (or zero). Treat them
as **compile-time constants** in Rust.

---

## 4. The quantisation curves the effects use

`09-dsp-eq.md §12` documents the `Qnt` module generally. The five effects use
exactly six of its curve types; the exact generators are repeated here because
every effect constant below derives from them.

`qntIToRCalc` is a pure table lookup: `out = real_array[midi - min_int_input]`
(`dsp/ptutil/Qnt/Qntitor2.cpp:559-581`). All the work is in the init.

### 4.1 `QNT_RESPONSE_LINEAR`, no quantise / no force / no snap

`dsp/ptutil/Qnt/Qntitor.cpp:167-178`:

```
scale    = (out_max - out_min) / (in_max - in_min)          = (out_max-out_min)/127
table[i] = out_min + scale * i,   i = 0..127
table[127] = out_max              (hard-set, kills roundoff)
```

Used by: Fidelity drive, Surround intensity, Lex room size, Lex motion rate/depth.

### 4.2 `QNT_RESPONSE_EXP`

`dsp/ptutil/Qnt/Qntitor.cpp:302-334`:

```
factor     = (out_max / out_min)^(1/in_max)      ← note: 1/127, not 1/(n-1)
table[0]   = out_min
table[127] = out_max
table[i]   = table[i-1] * factor,    i = 1..126     ⇒ table[i] = out_min·factorⁱ
```

Requires `out_min > 0` (`Qntitor.cpp:306-307`). The `i_snap_flag` argument is
ignored on this path — snapping only applies inside the LINEAR branch
(`Qntitor.cpp:183-204`). Used by: Ambience decay, Lex rolloff/damping frequency,
Maximizer release time-constant.

### 4.3 `QNT_RESPONSE_MAXI_BOOST_DSP`

`dsp/ptutil/Qnt/Qntitor.cpp:458-518`. Piecewise-linear in **dB**, then
converted to linear gain:

```
idx   0 … 59 :  dB = 0.1 · k,          k = 0..59      →  0.0 … 5.9
idx  60 … 89 :  dB = 6.0 + 0.2 · k,    k = 0..29      →  6.0 … 11.8
idx  90 …125 :  dB = 12.0 + 0.5 · k,   k = 0..35      → 12.0 … 29.5
idx 126      :  dB = table[127] = 30.0                (Qntitor.cpp:496)
idx 127      :  dB = 30.0                             (Qntitor.cpp:466)

then (MAXI_BOOST_DSP only):  table[i] = 10^(dB/20)     (Qntitor.cpp:511-517)
```

126 entries are produced by the three loops; entries 126 and 127 are patched.

### 4.4 `qnt2ndOrderButterworthInit` — 3 tables for one 2nd-order high-pass

`dsp/ptutil/Qnt/Qnt2But.cpp:41-117`:

```
factor   = (ω_max / ω_min)^(1/127)
ω[0]     = ω_min ;  ω[127] = ω_max ;  ω[i] = ω_min · factorⁱ
(gain[i], a1[i], a0[i]) = filtDesign2ndButHighPass(ω[i])
```

with (`dsp/ptutil/Filt/Fil12But.cpp:130-141`):

```c
omega2       = ω²
twoRoot2omega= 2√2 · ω
tmp          = 1 / (4 + ω² + 2√2·ω)
gain = 4·tmp
a1   = (8 − 2ω²)·tmp          /* already negated for the difference equation */
a0   = (2√2·ω − 4 − ω²)·tmp
```

Transfer function `H(z) = gain·(1 − 2z⁻¹ + z⁻²) / (1 − a1·z⁻¹ − a0·z⁻²)`
— a unity-passband-gain 2nd-order Butterworth **high-pass**, bilinear
(“linear transformed”), with the denominator coefficients stored **negated** so
the run-loop is all multiply-accumulate.

### 4.5 `qntIToRSimpleLowpassInit` — one-pole low-pass pole from a frequency table

`dsp/ptutil/Qnt/Qnt2But.cpp:130-184` feeds each entry of a frequency table
(in kHz, scaled by `r_scale_freq = 1.0e3`) through
`filtDesignSimple1rstLowPass` (`dsp/ptutil/Filt/Fil12But.cpp:37-46`):

```c
cos_om    = cos(ω)
root_calc = sqrt(cos_om² − 4·cos_om + 3)
a0        = 2 − cos_om − root_calc
```

Filter is `H(z) = (1 − a0)/(1 − a0·z⁻¹)`, i.e. `y[n] = (1−a0)·x[n] + a0·y[n−1]`,
matched at the exact −3 dB point (not the usual `exp(−ω)` approximation).

### 4.6 `qntIToRTimeConstantBeta`

`dsp/ptutil/Qnt/Qntitor2.cpp:502-552`:

```c
beta[i] = exp( −1 / (time_constant_ms[i] · 0.001 · sampling_freq) )
```

### 4.7 `qntIToBoostCutInit` — a 128-entry array of biquads

`dsp/ptutil/Qnt/QntitoBoostCut.cpp:37-164`:

```
boost_factor = (boost_max − boost_min) / 127
filt[i] = { r_center_freq, r_samp_freq, Q, boost = boost_min + i·boost_factor }
filtCalcParametric(&filt[i])            /* FILT_BOOST_CUT */
```

`filtCalcParametric` is documented in full in `09-dsp-eq.md §6`; §9 below
repeats only the parts the Bass effect depends on.

---

## 5. Effect 1 — Fidelity (Aural Enhancer / "Activator")

### 5.1 Algorithm: odd-harmonic exciter (sine waveshaper on a high-passed band)

This is **not** an EQ. It is a classic *aural exciter*: take the signal above a
corner frequency, drive it into a soft odd-symmetric non-linearity, and add the
generated harmonics back to the untouched dry signal.

`dsp/ptechDsp/Aural/Aural032/Auralp32.c:235-257` (left channel; right at
`:263-286` is identical):

```c
filtH1 = s->out1_minus1 * s->a1 + s->out1_minus2 * s->a0;        /* :235 */
s->out1_minus2 = s->out1_minus1;
filtH1 += (in1 + 1.0e-30f - 2.0f * s->in1_minus1 + s->in1_minus2) * s->gain; /* :238 */
s->out1_minus1 = filtH1;
s->in1_minus2  = s->in1_minus1;
s->in1_minus1  = in1;

filtH1 *= drive;                                                 /* :243 */

odd1 = (float)sin(filtH1);                                       /* :245 */

if (filtH1 > 0.0) even1 = filtH1; else even1 = 0.0f;             /* :251-254 */

out1 = in1 + (even * even1 + odd * odd1);                        /* :257 */
```

then, at `Auralp32.c:292`:

```c
kerWetDry(in1, in2, &(s->wet_gain), &(s->dry_gain), out1, out2);
/* dsp/ptutil/include/kerdelay.h:205-210:
   out  = out * wet ;  out += dry * in                                        */
```

### 5.2 The even-harmonic branch is dead

`aural_even` is set to `0.0` at init (`Play32.c:270`, `Auralp32.c:109`) and is
**never written from the host** (§3.6). `DSP_AURAL_EVEN_MAX_VALUE` is literally
`0.0` (`dsp/ptutil/include/c_aural.h:74`). So `even1`'s half-wave rectifier
contributes nothing. Do not port it — but note that it exists, because it is the
only reason the `if (filtH1 > 0.0)` branch is in the hot loop.

### 5.3 Collapsing the gain stage

`aural_odd = 1.5` (`Play32.c:269`, never written by the host);
`dry_gain = 0.622047`, `wet_gain = 0.377953` (`Play32.c:266-267`).
Note `0.622047 + 0.377953 = 1.000000` exactly. Therefore the whole stage is:

```
out = (in + 1.5·sin(drive·HP₂(in)))·0.377953 + in·0.622047
    = in + (0.377953 · 1.5) · sin(drive · HP₂(in))
    = in + 0.5669295 · sin(drive · HP₂(in))
```

**The Aural stage is unity-gain on the dry path.** The harmonic term has a fixed
ceiling of ±0.5669295 no matter how hard it is driven — that is what makes this
graceful instead of a fuzzbox. `DSP_AURAL_WET_BOOST = 2.0·0.75 = 1.5`
(`c_aural.h:72`) and `DSP_AURAL_ODD_MAX_VALUE = 1.0 · 1.5` (`c_aural.h:76`).

### 5.4 Drive mapping

`dfxp_InitStaticQnts` (`dsp/ptutil/dfxp/dfxpQnt.cpp:132-139`):

```c
qntIToRInit(&fidelity_qnt_hdl, ..., MIDI_MIN_VALUE, MIDI_MAX_VALUE,
            (realtype)DSP_AURAL_DRIVE_MIN_VALUE,                          /* 0.0 */
            (realtype)DSP_AURAL_DRIVE_MAX_VALUE * PLY_FIDELITY_INTENSITY_MAX_SCALE,
            IS_FALSE, 0, IS_FALSE, 0, IS_FALSE, QNT_RESPONSE_LINEAR);
```

```c
/* dsp/ptutil/include/c_aural.h:69-70 */
#define DSP_AURAL_DRIVE_MIN_VALUE 0.0
#define DSP_AURAL_DRIVE_MAX_VALUE (realtype)(TWO_PI/4.0 * 1.8 * 2.0 * 0.75)   /* 4.241150082 */
/* dsp/ptutil/include/c_play.h:81 */
#define PLY_FIDELITY_INTENSITY_MAX_SCALE (realtype)0.8
/* dsp/ptutil/include/pt_defs.h:39 */
#define TWO_PI 6.283185307
```

⇒ `drive_max = 4.241150082 × 0.8 = 3.39292007`, and

```
drive = midi × 3.39292007 / 127 = midi × 0.026715906
```

Communicated to `AURAL_DRIVE + DSP_PLAY_AURAL_PARAM_OFFSET` on front/rear/side/
center (`dfxpComm.cpp:551-562`); the subwoofer never gets it because the Aural
block is force-bypassed there.

At the drive ceiling the argument to `sin()` reaches ≈ 3.393 rad, i.e. just past
π — the waveshaper is being pushed **past its first fold**. That fold is the
character of the effect, and a naive `tanh` substitution will not sound the same.

| UI (0…10) | MIDI | `aural_drive` |
|---|---|---|
| 0 | 0 | 0.000000 |
| 1 | 13 | 0.347307 |
| 2 | 25 | 0.667898 |
| 3 | 38 | 1.015204 |
| **4.02 (default)** | **51** | **1.362511** |
| 5 | 64 | 1.709818 |
| 6 | 76 | 2.030409 |
| 7 | 89 | 2.377716 |
| 8 | 102 | 2.725022 |
| 9 | 114 | 3.045613 |
| 10 | 127 | 3.392920 |

### 5.5 The high-pass corner is fixed at MIDI 53

`dfxp_AuralCommunicateTune` (`dfxpComm.cpp:1250-1307`) hard-codes
`pc_tune = DSP_PLAY_AURAL_TUNE_MIDI = 53` (`c_play.h:60`) and pushes
`gain`, `a1`, `a0` from the three Butterworth tables built by
`dfxp_InitDynamicQnts_Aural` (`dfxpQnt.cpp:226-265`):

```c
omega_min = TWO_PI * DFXP_AURAL_CONTROL_HERTZ_MIN_VAL * internal_sampling_period;  /*   500 Hz */
omega_max = TWO_PI * DFXP_AURAL_CONTROL_HERTZ_MAX_VAL * internal_sampling_period;  /* 10000 Hz */
qnt2ndOrderButterworthInit(&gain, &a1, &a0, ..., 0, 127, omega_min, omega_max, QNT_RESPONSE_LINEAR);
```

(`dsp/ptutil/dfxp/u_dfxp.h:58-59` for the two frequencies.) The response-type
argument is ignored — `qnt2ndOrderButterworthInit` always uses the exponential
sweep (`Qnt2But.cpp:58`).

```
factor = (10000/500)^(1/127) = 20^(1/127) = 1.023868851
f(53)  = 500 × 1.023868851⁵³ = 1745.499 Hz     ← sample-rate independent
```

Resulting coefficients:

| `internal_sampling_freq` | ω | `gain` | `a1` | `a0` |
|---|---|---|---|---|
| 44100 | 0.2486829 | 0.839410 | 1.652862 | −0.704777 |
| 48000 | 0.2284787 | 0.851343 | 1.680463 | −0.724908 |

> The init constants in `Play32.c:272-274` / `Auralp32.c:111-113`
> (`gain 0.788950, a1 1.53285, a0 −0.622949`) correspond to ≈ 2372 Hz — they are
> the "Quick preset 1" leftovers and are overwritten by
> `dfxp_AuralCommunicateTune` on the very first `dfxpCommunicateAll`
> (`dfxpComm.cpp:116-117, 324-325`). Do not port them.

### 5.6 State and buffers

Per stereo instance: **8 floats**, no delay lines, no allocation.
`out1_minus1, out1_minus2, in1_minus1, in1_minus2` and the `2`-suffixed twins
(`c_aural.h:57-65`), zeroed on `DSPS_INIT_PARAMS | DSPS_ZERO_MEMORY`
(`Auralp32.c:118-129`). `DSPS_SOFT_MEM_AURAL_ENHANCER_LENGTH == 0`
(`c_dsps.h:81`).

The `+1.0e-30f` denormal bias at `Auralp32.c:238` is essential on x86 — without
it the recursive high-pass drops into denormal territory during silence and the
loop cost explodes.

### 5.7 CPU cost

Per stereo sample: 2 × (3 mul + 4 add) for the biquads, **2 × `sin()`**, 2
compares, 2 mul + 2 add for the mix, 4 mul + 2 add for `kerWetDry`. The two
`sin()` calls dominate — on x86-64 a scalar `sin` is ~20–40 cycles, so this
effect is roughly **half the total cost of the whole chain**.

Optimisation for the port: since `|filtH| ≤ drive_max ≈ 3.393 < 2π`, a 7th-order
odd minimax polynomial or a 4-way SIMD range-reduced sine reproduces `sinf` to
well under −120 dBFS. The source itself notes a cheaper option
(`Auralp32.c:246-249`):

```c
/* odd1 = filtH1 - (filtH1*filtH1*filtH1) * (1.0/6.0);  works extremely well
   if the input does not exceed 1.0 */
```

but that Taylor form is **only valid for |x| ≤ 1**, and drive goes to 3.39 — it
diverges badly past π/2. Do not use it.

### 5.8 Rust sketch

```rust
pub struct Fidelity {
    // coefficients (recomputed on sample-rate change only)
    gain: f32, a1: f32, a0: f32,      // 2nd-order Butterworth HP @ 1745.499 Hz
    drive: f32,                        // 0.0 ..= 3.392_920_1
    st: [[f32; 4]; 2],                 // [ch][out_m1, out_m2, in_m1, in_m2]
}

const AURAL_HARMONIC_GAIN: f32 = 0.377_953 * 1.5;   // = 0.566_929_5
const AURAL_HP_HZ: f32 = 1745.499;                  // 500 * 20f32.powf(53.0/127.0)
const AURAL_DRIVE_MAX: f32 = 3.392_920_1;           // TWO_PI/4*1.8*2*0.75*0.8

impl Fidelity {
    pub fn set_coeffs(&mut self, fs: f32) {
        let w = core::f32::consts::TAU * AURAL_HP_HZ / fs;
        let w2 = w * w;
        let r2w = 2.0 * core::f32::consts::SQRT_2 * w;
        let t = 1.0 / (4.0 + w2 + r2w);
        self.gain = 4.0 * t;
        self.a1 = (8.0 - 2.0 * w2) * t;
        self.a0 = (r2w - 4.0 - w2) * t;
    }
    #[inline]
    pub fn set_value01(&mut self, v: f32) {
        let midi = (v.clamp(0.0, 1.0) * 127.0).round();
        self.drive = midi * (AURAL_DRIVE_MAX / 127.0);
    }
    #[inline]
    fn ch(&mut self, c: usize, x: f32) -> f32 {
        let s = &mut self.st[c];
        let mut hp = s[0] * self.a1 + s[1] * self.a0;
        s[1] = s[0];
        hp += (x + 1.0e-30 - 2.0 * s[2] + s[3]) * self.gain;
        s[0] = hp; s[3] = s[2]; s[2] = x;
        x + AURAL_HARMONIC_GAIN * fast_sin(hp * self.drive)
    }
}
```

---

## 6. Effect 2 — Ambience (the "Lex" plate reverb)

### 6.1 Algorithm: Dattorro figure-of-eight tank

`Lex32.c` is a direct descendant of the Lexicon 224 / Dattorro *Effect Design
Part 1* plate topology, hand-unrolled into one shared circular buffer.

```
 (in1 + in2)                                     [mono sum, Lex32.c:341]
     │
     ▼
 ┌──────────────┐  pre-delay ring, 100 ms allocated,  TAP = pre_delay = 1 sample
 │  PRE-DELAY   │  Lex32.c:330-342   (pre_delay is NEVER written by the host → 1)
 └──────────────┘
     │
     ▼
 ┌──────────────┐  y = (1−bw)·x + bw·y[-1]        one-pole LP   Lex32.c:345-346
 │  BANDWIDTH   │  bw = a0(8161.04 Hz)
 └──────────────┘
     │
     ▼  INPUT DIFFUSER — 4 cascaded lattice allpasses    Lex32.c:354-397
 ┌──────┐  ┌──────┐  ┌──────┐  ┌──────┐
 │ LAT1 │→ │ LAT2 │→ │ LAT3 │→ │ LAT4 │→ input_diffuser_out
 │g=.75 │  │g=.75 │  │g=.625│  │g=.625│
 │4.77ms│  │3.595 │  │12.73 │  │9.31  │
 └──────┘  └──────┘  └──────┘  └──────┘
     │
     ├───────────────────────────┬─────────────────────────────┐
     ▼ BRANCH A                  │                             ▼ BRANCH B
 + decay·D4_out ◄────────────────┼──────────────┐        + decay·D2_tap3
     │                           │              │              │
     ▼                           │              │              ▼
 ┌────────────┐ decay diffuser   │              │       ┌────────────┐
 │   LAT5     │ g = lat5_coeff   │              │       │   LAT7     │ g = lat5_coeff
 │  22.6 ms   │ = 0.70           │              │       │  30.5 ms   │ = 0.70
 └────────────┘ (modulated in    │              │       └────────────┘
     │           DSPFX only —    │              │             │
     ▼           fixed here)     │              │             ▼
 ┌────────────────────────┐      │              │    ┌────────────────────────┐
 │ D1  4-tap delay        │      │              │    │ D3  4-tap delay        │
 │ 10.1/66.9/121.9/149.6ms│      │              │    │ 10.1/70.9/99.9/141.7ms │
 └────────────────────────┘      │              │    └────────────────────────┘
   out1 −= tap2                  │              │      out1 += tap1 + tap3
   out2 += tap1 + tap3           │              │      out2 −= tap2
     │ (tap4 = through)          │              │        │ (tap4 = through)
     ▼                           │              │        ▼
 ┌────────────┐ one-pole LP      │              │   ┌────────────┐ one-pole LP
 │  DAMPING 1 │ ×decay           │              │   │  DAMPING 2 │ ×decay
 └────────────┘                  │              │   └────────────┘
     │                           │              │        │
     ▼                           │              │        ▼
 ┌────────────────────┐          │              │   ┌────────────────────┐
 │ LAT6 tapped AP     │          │              │   │ LAT8 tapped AP     │
 │ g=lat6_coeff 60.5ms│          │              │   │ g=lat6_coeff 89.2ms│
 │ taps 6.28 / 41.26  │          │              │   │ taps 11.25 / 64.3  │
 └────────────────────┘          │              │   └────────────────────┘
   out1 −= tap1                  │              │     out1 −= tap2
   out2 −= tap2                  │              │     out2 −= tap1
     │                           │              │        │
     ▼                           │              │        ▼
 ┌────────────────────┐          │              │   ┌────────────────────┐
 │ D2  3-tap delay    │          │              │   │ D4  3-tap delay    │
 │ 35.8/89.8/125.0 ms │          │              │   │ 4.065/67.1/106.3ms │
 └────────────────────┘          │              │   └────────────────────┘
   out1 −= tap1                  │              │     out1 += tap2
   out2 += tap2                  │              │     out2 −= tap1
   tap3 ──────────── feeds BRANCH B ────────────┘     tap3 = D4_out ─── feeds BRANCH A
                                 └──────────────────────────────────────┘

 out1,out2 ×= 0.6·0.5 = 0.3                                     Lex32.c:667-668
 out = out·wet_gain + in·dry_gain          (kerWetDry)          Lex32.c:676
```

Line references for each block: pre-delay `:330-342`; bandwidth LP `:345-346`;
LAT1-4 `:354-397`; LAT5 `:421-466`; D1 `:468-491`; damping 1 `:494-497`;
LAT6 `:499-521`; D2 `:523-542`; LAT7 `:544-588`; D3 `:590-613`; damping 2
`:615-619`; LAT8 `:620-643`; D4 `:645-665`; output scale `:667-668`; mono fold
`:670-675`; wet/dry `:676`.

### 6.2 The single-ring trick (important for the port)

There is **one** circular buffer (`MasterStart … MasterEnd`,
`Lex32.c:172-239`). Each stage advances `s->ptr` by *its own length* and wraps
by subtracting `MasterLen`. The first advance adds `+1`
(`Lex32.c:333`). Summing the advances:

```
(pre+1) + lat1 + lat2 + lat3 + lat4 + lat5_maxlen + D1_tap4 + lat6
        + D2_tap3 + lat7_maxlen + D3_tap4 + lat8 + D4_tap3   ==  MasterLen + 1
```

so the net movement per sample is exactly **+1**. Every stage reads its own
"oldest" slot and overwrites it — a classic single-buffer multi-delay. `next_out`
carries the oldest value of the *next* stage across, saving one read per stage
(`Lex32.c:340, 361, 372, 384, 463, …`).

Recomputed ring sizes (roomsize = 1.0039370, see §6.4):

| `internal_sampling_freq` | pre | lat1 | lat2 | lat3 | lat4 | lat5max | D1t4 | lat6 | D2t3 | lat7max | D3t4 | lat8 | D4t3 | **MasterLen** |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 44100 | 4410 | 210 | 158 | 561 | 410 | 1089 | 6623 | 2678 | 5534 | 1439 | 6273 | 3949 | 4706 | **38040** (862.59 ms) |
| 48000 | 4800 | 228 | 172 | 611 | 446 | 1186 | 7209 | 2915 | 6023 | 1566 | 6828 | 4298 | 5122 | **41404** (862.58 ms) |

This matches the comment in `dsp/ptutil/include/c_lex.h:66-69`
("Total nominal delay … = 859.805 ms").

All lengths are `(long)` **truncations**, not rounds — e.g.
`lat1 = (long)(44100 × 0.00477) = (long)210.357 = 210`
(`Lex32.c:177`). Reproduce the truncation; rounding shifts the modal density.

`lat5_maxlen = (unsigned long)lat5_dly_len + (unsigned long)(fs·0.002) + 1`
(`Lex32.c:197`) reserves 2 ms of modulation headroom
(`LEX_MAX_MODULATION_DELAY`, `c_lex.h:63`) that the DFX build never uses.

### 6.3 Delay constants

`dsp/ptutil/include/c_lex.h:57-99` (all in seconds):

| Symbol | ms | Line |
|---|---|---|
| `LEX_PREDELAY_LEN` | 100.0 | :65 |
| `LAT1_LEFT_DELAY_LEN` | 4.77 | :70 |
| `LAT2_LEFT_DELAY_LEN` | 3.595 | :71 |
| `LAT3_LEFT_DELAY_LEN` | 12.73 | :72 |
| `LAT4_LEFT_DELAY_LEN` | 9.31 | :73 |
| `LAT5_LEFT_DELAY_LEN_NOMINAL` | 22.6 | :74 |
| `D1_LEFT_TAP1_DELAY` | **10.1** (was 11.9) | :77 |
| `D1_LEFT_TAP2_DELAY` | 66.9 | :78 |
| `D1_LEFT_TAP3_DELAY` | 121.9 | :79 |
| `D1_LEFT_TAP4_DELAY` | 149.6 | :80 |
| `LAT6_LEFT_TAP1_DELAY` | 6.28 | :81 |
| `LAT6_LEFT_TAP2_DELAY` | 41.26 | :82 |
| `LAT6_LEFT_DELAY_LEN` | 60.5 | :83 |
| `D2_LEFT_TAP1_DELAY` | 35.8 | :84 |
| `D2_LEFT_TAP2_DELAY` | 89.8 | :85 |
| `D2_LEFT_TAP3_DELAY` | 125.0 | :86 |
| `LAT7_LEFT_DELAY_LEN_NOMINAL` | 30.5 | :87 |
| `D3_LEFT_TAP1_DELAY` | **10.1** (was 8.93) | :90 |
| `D3_LEFT_TAP2_DELAY` | 70.9 | :91 |
| `D3_LEFT_TAP3_DELAY` | 99.9 | :92 |
| `D3_LEFT_TAP4_DELAY` | 141.7 | :93 |
| `LAT8_LEFT_TAP1_DELAY` | 11.25 | :94 |
| `LAT8_LEFT_TAP2_DELAY` | 64.3 | :95 |
| `LAT8_LEFT_DELAY_LEN` | 89.2 | :96 |
| `D4_LEFT_TAP1_DELAY` | 4.065 | :97 |
| `D4_LEFT_TAP2_DELAY` | 67.1 | :98 |
| `D4_LEFT_TAP3_DELAY` | 106.3 | :99 |

Everything except `pre`, `lat1..lat4` is multiplied by
`r_roomsize = roomsize × samp_freq` (`Lex32.c:192`) — the input diffuser is
**not** scaled by room size, only the tank.

### 6.4 Fixed parameters (host-written but constant)

| DSP field | Value | Where computed |
|---|---|---|
| `roomsize` | **1.0039370078740** | `dfxp_LexCommunicateSize`, `dfxpComm.cpp:1314-1353`, MIDI 64 through a LINEAR qnt over `[DSP_LEX_ROOM_SIZE_MIN_VALUE 0.5, MAX 1.5]` (`c_lex.h:50-51`, `dfxpQnt.cpp:177-184`). `0.5 + 64·(1.0/127) = 1.00393700787`. |
| `bandwidth` (input LP pole) | see table | `dfxp_LexCommunicateRolloff`, `dfxpComm.cpp:1360-1397`, MIDI 89 |
| `one_minus_bandwidth` | `1 − bandwidth` | `dfxpComm.cpp:1387-1394` |
| `damping` (in-loop LP pole) | see table | `dfxp_LexCommunicateDamping`, `dfxpComm.cpp:1404-1442`, MIDI 81 |
| `one_minus_damping` | `1 − damping` | `dfxpComm.cpp:1432-1439` |
| `modulation_depth`, `modulation_freq` | written but **unused** in the DFX build | `dfxpComm.cpp:1449-1514` |
| `lat1_coeff` | 0.75 | `Play32.c:304`, never written |
| `lat3_coeff` | 0.625 | `Play32.c:305`, never written |
| `lat5_coeff` | 0.70 | `Play32.c:306`, never written |
| `pre_delay` | 1 sample | `Play32.c:316`, never written |

Note the naming is crossed: `LEX_ROLLOFF` lands on the struct field `bandwidth`
and `LEX_DAMPING` on `damping` (`c_lex.h:131-140` vs `:207-219`). Both MIDI
values index the *same* exponential 1…20 kHz table, because
`LEX_HIGH_FREQ_ROLLOFF_MIN/MAX` and `LEX_HIGH_FREQ_DECAY_MIN/MAX` are all
1.0/20.0 (`dsp/ptutil/include/dspfxp_studioverb.h:30-34`, via `c_lex.h:27-30`).

```
factor  = 20^(1/127) = 1.023868851
f(89)   = 1.0 × factor⁸⁹ =  8.161038563 kHz   → bandwidth
f(81)   = 1.0 × factor⁸¹ =  6.757573602 kHz   → damping
```

| `internal_sampling_freq` | `bandwidth` | `1−bandwidth` | `damping` | `1−damping` |
|---|---|---|---|---|
| 44100 | 0.350108 | 0.649892 | 0.408288 | 0.591712 |
| 48000 | 0.375810 | 0.624190 | 0.435258 | 0.564742 |

> **Validation.** `Play32.c:308-311` hard-codes `damping 0.408290`,
> `one_minus_damping 0.591710`, `bandwidth 0.350110`,
> `one_minus_bandwidth 0.649890` "for 44.1 kHz". The recomputed values agree to
> 5 decimal places. The derivation chain in §4.5 + §4.2 is therefore correct.

### 6.5 The three parameters the Ambience knob actually moves

`dfxp_CommunicateAmbience` (`dfxpComm.cpp:571-679`):

```c
pc_liveliness = midi_from_registry;
if (music_mode != MUSIC1)                                        /* always true */
    pc_liveliness = (int)(pc_liveliness * 0.34);                 /* :596-597   */

pc_size = DSP_PLAY_LEX_ROOM_SIZE_MIDI;                           /* 64         */
qntIToRCalc(room_size_qnt_hdl, pc_size, &roomsize);              /* 1.0039370  */
qntIToRCalc(ambience_qnt_hdl,  pc_liveliness, &dsp_decay);       /* EXP curve  */
dsp_decay = pow(dsp_decay, roomsize);                            /* :611       */

dsp_lat6_coeff = dsp_decay + 0.15;                               /* :613       */
if (dsp_lat6_coeff < 0.25) dsp_lat6_coeff = 0.25;                /* :614-615   */
if (dsp_lat6_coeff > 0.5 ) dsp_lat6_coeff = 0.5;                 /* :616-617   */

if (pc_liveliness > 40) { wet = 0.21*1.3;  dry = 0.69*1.3; }     /* :621-626   */
else { wet = (pc_liveliness-12) * (1.0/(40-12)) * (0.21*1.3);    /* :630       */
       dry = 0.897 + ((40-pc_liveliness) * (1.0/(40-12))) * (1.0-0.897); } /* :631 */
```

The decay curve (`dfxpQnt.cpp:144-151`) is `QNT_RESPONSE_EXP` over
`[PLY_DECAY_MIN_VALUE 0.095, PLY_DECAY_MAX_VALUE 0.95]` (`c_play.h:100-101`):

```
factor = (0.95/0.095)^(1/127) = 10^(1/127) = 1.0182959483
raw[i] = 0.095 · factorⁱ
decay  = raw[i] ^ 1.0039370078740
```

`PLY_AMBIENCE_BOOST_FACTOR = 1.3` (`c_play.h:96`) is the `1.3` in the wet/dry
constants. Wet/dry are continuous at `pc_liveliness == 40`
(`wet 0.273`, `dry 0.897`) and at 12 (`wet 0.0`, `dry 1.0`).

### 6.6 Bypass rule

`dfxp_CommAmbienceBypass` (`dfxpComm.cpp:1662-1709`):

```c
if (ambience_bypass || (pc_liveliness <= DFXP_MIN_EFFECTIVE_MIDI_AMBIENCE))  /* 12 */
    ambience_bypass = IS_TRUE;
```

Note this uses the **unscaled** registry MIDI value (it calls
`dfxp_GetKnobValue_MIDI` directly at `:1675` and does *not* apply the 0.34
music-mode factor). Meanwhile `dfxp_CommunicateAmbience` *does* scale, and also
calls `dfxp_CommAmbienceBypass` at its end (`:675`). So:

* the **bypass flag** uses `midi ≤ 12` → UI ≤ 0.94;
* the **wet gain** uses `int(midi·0.34) ≤ 12` → UI ≤ 3.07 gives `wet ≤ 0.0`
  (actually *negative* below UI 3.07 — see §12.3).

### 6.7 Effective mapping under the shipped MUSIC2 mode

| UI (0…10) | MIDI | eff = `int(midi·0.34)` | `decay` | `lat6_coeff` | `wet_gain` | `dry_gain` | bypassed? |
|---|---|---|---|---|---|---|---|
| 0 | 0 | 0 | 0.094124 | 0.2500 | −0.117000 | 1.044143 | **yes** |
| 1 | 13 | 4 | 0.101232 | 0.2512 | −0.078000 | 1.029429 | no (knob>12? no → **yes**) |
| 2 | 25 | 8 | 0.108878 | 0.2589 | −0.039000 | 1.014714 | **yes** |
| 3 | 38 | 12 | 0.117101 | 0.2671 | 0.000000 | 1.000000 | no — but wet = 0 |
| **4.02** | **51** | **17** | **0.128258** | **0.2783** | **0.048750** | **0.981607** | no |
| 5 | 64 | 21 | 0.137944 | 0.2879 | 0.087750 | 0.966893 | no |
| 6 | 76 | 25 | 0.148363 | 0.2984 | 0.126750 | 0.952179 | no |
| 7 | 89 | 30 | 0.162499 | 0.3125 | 0.175500 | 0.933786 | no |
| 8 | 102 | 34 | 0.174771 | 0.3248 | 0.214500 | 0.919071 | no |
| 9 | 114 | 38 | 0.187970 | 0.3380 | 0.253500 | 0.904357 | no |
| 10 | 127 | 43 | 0.205880 | 0.3559 | 0.273000 | 0.897000 | no |

Bypass turns on at `midi ≤ 12`, i.e. UI ≤ 0.94. Between UI 0.95 and 3.07 the
reverb *runs* but `wet_gain ≤ 0` — the tank is computed and then multiplied by a
non-positive number. See §12.3.

Because the 0.34 factor caps `eff` at 43, the shipped app never uses a decay
above **0.206**; the reverb is a short, dense ambience, not a hall.

### 6.8 Memory and state

```c
/* dsp/ptutil/include/c_dsps.h:92 */
#define DSPS_SOFT_MEM_LEX_LENGTH (long)(2*(8192+1) + 1.5*(860*0.001*DSPS_SOFT_MAX_SAMP_FREQ))
/* DSPS_SOFT_MAX_SAMP_FREQ = 96000.00  (c_dsps.h:69)  ⇒ 16386 + 123840 = 140226 floats */
```

Layout inside that block (`Lex32.c:123-124, 172`):

```
[0 … 8192]              osc_mult_table   (8193 floats, built and never read)
[8193 … 16385]          osc_plus_table   (8193 floats, built and never read)
[16386 … 16386+MasterLen-1]   the delay ring
```

Zeroing skips the tables: `for(i=0; i<(LEN − 2·LEX_NUM_OSC_PTS); i++)
fp_memory[i + 2·LEX_NUM_OSC_PTS] = 0.0;` (`Lex32.c:160-161`).

Scalar state: `old_bandwidth_val_l`, `old_damp_val1_l`, `old_damp_val2_l`,
`D4_out`, plus the `ptr` cursor (`c_lex.h:177, 194, 199, 201-203`).

`comMemReInitialize` (`ComMem.cpp:56-70`) is called after every room-size write
(`dfxpComm.cpp:1343-1350`) with `DSPS_RE_INIT_MEMORY`, which recomputes every
pointer and length (`Lex32.c:168-240`) **without clearing the ring**. Since room
size is a constant in this build, this happens once per format change.

> **Port decision.** Drop both oscillator tables (32 KB saved). Size the ring
> for the actual sample rate rather than the 96 kHz worst case: 41404 floats at
> 48 kHz = 162 KB instead of 548 KB. Use `Vec<f32>` allocated in
> `prepare(sample_rate)` on the control thread, never in the process callback.

### 6.9 CPU cost

Per stereo sample: 13 ring stages, each a bounds-compare + wrap + 1–2 reads +
1 write; ~26 multiplies, ~40 adds; 13 extra tap reads. No transcendental, no
division. The real cost is **memory**: 13 scattered accesses per sample spread
over a 162 KB ring at 48 kHz — well past L1, so expect L2-bound behaviour.
Roughly 25–30 % of the chain cost.

### 6.10 Rust sketch (ring discipline)

```rust
pub struct Lex {
    ring: Vec<f32>,          // MasterLen entries
    p: usize,                // cursor, advances net +1 per sample
    len: usize,
    pre_len: usize, pre_tap: usize,            // pre_tap = 1
    lat: [usize; 4],                            // lat1..lat4 lengths
    lat5_max: usize, lat5: usize,
    d1: [usize; 4], lat6_len: usize, lat6_tap: [usize; 2], d2: [usize; 3],
    lat7_max: usize, lat7: usize,
    d3: [usize; 4], lat8_len: usize, lat8_tap: [usize; 2], d4: [usize; 3],
    g1: f32, g3: f32, g5: f32,                  // 0.75, 0.625, 0.70
    g6: f32,                                    // lat6_coeff, from decay
    decay: f32, bw: f32, damp: f32,
    wet: f32, dry: f32,
    s_bw: f32, s_d1: f32, s_d2: f32, d4_out: f32,
}

impl Lex {
    #[inline] fn adv(&mut self, n: usize) { self.p += n; if self.p >= self.len { self.p -= self.len; } }
    #[inline] fn tap(&self, back: usize) -> f32 {
        let i = if self.p >= back { self.p - back } else { self.p + self.len - back };
        self.ring[i]
    }
    #[inline] fn allpass(&mut self, adv: usize, g: f32, x: f32, carry: &mut f32) -> f32 {
        self.adv(adv);
        let d_out = *carry;
        let d_in = x - g * d_out;
        *carry = self.ring[self.p];
        self.ring[self.p] = d_in;
        d_out + g * d_in
    }
    // decay diffuser: sign convention is INVERTED w.r.t. allpass (Lex32.c:462-465)
    #[inline] fn decay_diffuser(&mut self, adv: usize, dly: usize, g: f32, x: f32, carry: &mut f32) -> f32 {
        self.adv(adv);
        let d_out = self.tap(dly);
        let d_in = x + g * d_out;
        *carry = self.ring[self.p];
        self.ring[self.p] = d_in;
        d_out - g * d_in
    }
}
```

Note the `+1` on the very first advance (`Lex32.c:333`) — omit it and the ring
drifts by one sample per block.

---

## 7. Effect 3 — Surround (the "Wide" widener)

### 7.1 The shipped algorithm is 12 lines long

`dsp/ptechDsp/wide/Wide32/Wide32.c:159-163` carries a banner:

```
//   Theremino V2.0.3 - SURROUND SIMPLIFIED AND IMPROVED
//  All the initializations are done here and the precedent initializations are unused.
```

Coefficients, computed **once per buffer** at `Wide32.c:214-215`:

```c
gainFactorSide         = 1 + 3.0 * s->intensity;   // (3 to 5) This is the surround intensity
gainFactorCompensation = 1 - 0.3 * s->intensity;   // (0.3) This decreases also the mono signal
```

Per-sample (`Wide32.c:230-236`):

```c
mono_signal  = (in1 + in2) * 0.5f;
l_minus_mono =  in1 - mono_signal;      /* = +S/2 */
r_minus_mono =  in2 - mono_signal;      /* = −S/2 */

mono_signal *= gainFactorCompensation;
out1 = mono_signal + gainFactorSide * l_minus_mono;
out2 = mono_signal + gainFactorSide * r_minus_mono;
```

i.e. a pure **mid/side gain widener**:

```
M = (L+R)/2                     S = (L−R)/2
M' = M · (1 − 0.3·i)            S' = S · (1 + 3·i)
L' = M' + S'                    R' = M' − S'
```

Mono fold (`Wide32.c:239-244`): if `!stereo_in_flag`, `in2 = 0` and both outputs
are halved — which, combined with the mono write macro summing them
(`dutio.h:421-425`), restores unity. `bypass_flag` (`Wide32.c:246-250`) is set
to 0 at init and never written; the host bypasses by not calling the block at
all (`Play32.c:678-685`).

There is **no `kerWetDry` call** in `Wide32.c` — the widener output is the stage
output directly.

### 7.2 Intensity mapping

`dfxp_InitStaticQnts` (`dfxpQnt.cpp:164-171`): LINEAR over
`[DSP_WID_INTENSITY_MIN_VALUE 0.0, DSP_WID_INTENSITY_MAX_VALUE × PLY_WIDENER_BOOST_MAX_SCALE]`
= `[0.0, 1.0 × 0.7]` (`c_wid.h:29-30`, `c_play.h:93`).

```
intensity = midi × 0.7 / 127 = midi × 0.005511811
```

Written to `DSP_WID_INTENSITY + DSP_PLAY_WIDENER_PARAM_OFFSET` on front, rear
and side only (`dfxp_CommunicateSpaciousness`, `dfxpComm.cpp:786-818`).

| UI (0…10) | MIDI | `intensity` | side gain `1+3i` | mid gain `1−0.3i` |
|---|---|---|---|---|
| 0 | 0 | 0.000000 | 1.0000 | 1.0000 |
| 1 | 13 | 0.071654 | 1.2150 | 0.9785 |
| **2.05 (default)** | **26** | **0.143307** | **1.4299** | **0.9570** |
| 3 | 38 | 0.209449 | 1.6283 | 0.9372 |
| 4 | 51 | 0.281102 | 1.8433 | 0.9157 |
| 5 | 64 | 0.352756 | 2.0583 | 0.8942 |
| 6 | 76 | 0.418898 | 2.2567 | 0.8743 |
| 7 | 89 | 0.490551 | 2.4717 | 0.8528 |
| 8 | 102 | 0.562205 | 2.6866 | 0.8313 |
| 9 | 114 | 0.628346 | 2.8850 | 0.8115 |
| 10 | 127 | 0.700000 | **3.1000** | **0.7900** |

At the maximum the side signal is boosted by **+9.83 dB** while mid is cut by
−2.05 dB. On a mono source `S = 0` and the effect is a flat **−2.05 dB** at
full — worth a note in the UI. The source comment says "(3 to 5)" but the
`PLY_WIDENER_BOOST_MAX_SCALE = 0.7` clamp means the side gain tops out at 3.1,
not 4.0.

### 7.3 The dead historical widener

`Wide32.c:71-156` still allocates three delay lines
(`dly_start_l`, `dly_start_r`, `dly_start_mono`, each `DSPS_SOFT_MEM_WIDE_LENGTH/3`
= 2409 floats) and `dfxp_WidCommunicateDispersion` / `…FreqThreshold`
(`dfxpComm.cpp:1559-1655`) still push `dispersion_l/r`, `gain/a1/a0` every
communicate. None of it is read by the 2025 process function.

For reference, the pre-2025 algorithm survives verbatim in
`dsp/ptechDsp/wide/Wide16/Wide16.c:222-291` — a **cross-fed dispersion
widener**:

```c
mono_sig = (in1 + in2) * 0.5;  l_minus_mono = in1 - mono_sig;  r_minus_mono = in2 - mono_sig;
filtH1 = HP2(l_minus_mono);    filtH2 = HP2(r_minus_mono);        /* :232-246 */
dly_l_out = delay(filtH1, dispersion_l);                          /* :249-253 */
dly_r_out = delay(filtH2, dispersion_r);                          /* :255-259 */
dly_mono  = (center_depth > 1) ? delay(mono_sig, center_depth) : mono_sig;  /* :262-271 */
dly_mono *= center_gain;
out1 = l_minus_mono + dly_mono - 5.0*intensity*(width*dly_r_out + reverse_width*dly_l_out);
out2 = r_minus_mono + dly_mono - 5.0*intensity*(width*dly_l_out + reverse_width*dly_r_out);
out *= master_gain;                                               /* :275-278 */
```

with `dispersion_r` from a LINEAR qnt over `[1 + 0.5 ms·fs, 25 ms·fs]` samples
(`WID_DISPERSION_MIN_MS 0.5`, `MAX_MS 25.0`, `c_wid.h:21-22`;
`dfxpQnt.cpp:457-472`) at MIDI 23 (`DSP_PLAY_WID_DISPERSION_MIDI`,
`c_play.h:67`), `dispersion_l = dispersion_r × 0.793651`
(`DSP_WID_LEFT_RIGHT_DISPERSION_FACTOR`, `c_wid.h:45`), and a 2nd-order
Butterworth high-pass at MIDI 0 over `[100 Hz, 2000 Hz]`
(`WID_FREQ_THRESHOLD_MIN/MAX`, `c_wid.h:26-27`; `dfxpQnt.cpp:474-505`) — i.e.
100 Hz, so the side path is high-passed at 100 Hz before being cross-delayed.

**Do not port the historical version.** Port the 2025 mid/side widener; mention
the old one in the changelog only.

### 7.4 State and cost

State: none that matters (the eight `*_minus*` fields are only touched by the
dead path). Cost: 2 mul for the coefficients per buffer, then per sample
3 add + 1 mul (mono) + 2 sub + 2 mul + 2 add ≈ **10 flops**. Negligible —
under 3 % of the chain.

### 7.5 Rust sketch

```rust
pub struct Surround { side: f32, mid: f32 }
impl Surround {
    #[inline] pub fn set_value01(&mut self, v: f32) {
        let i = (v.clamp(0.0,1.0) * 127.0).round() * (0.7 / 127.0);
        self.side = 1.0 + 3.0 * i;
        self.mid  = 1.0 - 0.3 * i;
    }
    #[inline] pub fn process(&self, l: f32, r: f32) -> (f32, f32) {
        let m = (l + r) * 0.5;
        let s = l - m;                 // == (l-r)/2
        let m = m * self.mid;
        (m + self.side * s, m - self.side * s)
    }
}
```

---

## 8. Effect 4 — Dynamic Boost (the "Maximizer" / Optimizer)

### 8.1 Algorithm: slow auto-gain + fast look-ahead brick-wall limiter

Two loops in one. Neither is a conventional compressor — there is no ratio and
no knee.

```
              ┌──────────────────────────────────────────────┐
   in1 ──────►│ in²  ──►  one-pole LP, fc = 0.1 Hz (τ≈1.59 s)│──► level
   (LEFT ONLY)└──────────────────────────────────────────────┘      │
                                                                    ▼ sqrt
                                                              sqrt_level
                                                                    │
              gain_boost := (gain_boost·sqrt_level > target_level)   │
                            ? max(target_level / sqrt_level, 1.06)   │
                            : gain_boost                             │
                                        │
   in1,in2 ──► × gain_boost × max_output ──► [look-ahead ring, max_delay samples]
                                 │                       │
                                 │                       ▼ dly_out (delayed by max_delay)
                                 ▼ new_abs = |written|
                        ┌─────────────────────┐
                        │  ENVELOPE FOLLOWER  │  linear attack ramp over max_delay
                        │  (per channel)      │  + exponential release (beta)
                        └─────────────────────┘
                                 │ env
                                 ▼
              out = (env > max_output) ? dly_out · max_output/env : dly_out
```

### 8.2 The auto-gain (RMS) loop

`Maxi32.c:258-294`:

```c
in_sqr      = in1 * in1;                       /* LEFT CHANNEL ONLY  :259   */
s->level    = s->level * s->a0 + in_sqr * s->filt_gain;          /* :267   */
sqrt_level  = sqrtf(s->level);                                   /* :269   */

result = s->gain_boost * sqrt_level;
if (result > s->target_level) {
    gain_boost = s->target_level / sqrt_level;                   /* :285   */
    if (gain_boost < 1.06f) gain_boost = 1.06f;                  /* :288-289 */
} else
    gain_boost = s->gain_boost;                                  /* :293   */
```

* `s->level` is a **`double`** (`c_max.h:123-125`) — the comment at
  `Maxi32.c:116-117` says "Trying double versions to see if normalization
  problem goes away". At a pole of 0.99998575 a single-precision accumulator
  loses the update entirely. **The port must use `f64` here.**
* The estimator is a mean-square, so `sqrt_level` is an **RMS**, not a peak.
* Only the left channel feeds it (`Maxi32.c:258-259`). Hard-panned right-channel
  content is invisible to the auto-gain. Reproduce or fix deliberately.
* The `1.06` floor (`Maxi32.c:287-289`, commented "11/4/04 Modifications to help
  fix volume pumping") means the auto-gain never backs off below +0.5 dB.

Level filter design (`Maxi32.c:119-137`), same `filtDesignSimple1rstLowPass`
form as §4.5 with `MAXIMIZE_LEVEL_FILT_CUTOFF = 0.1` Hz (`c_max.h:57`):

| `fs` | `a0` | `filt_gain = 1−a0` | τ = −1/(fs·ln a0) |
|---|---|---|---|
| 44100 | 0.9999857525 | 1.4247 × 10⁻⁵ | **1.5915 s** |
| 48000 | 0.9999869101 | 1.3090 × 10⁻⁵ | **1.5915 s** |

`target_level = MAXIMIZE_TARGET_LEVEL_SETTING = 0.32` (`c_max.h:53`), pushed by
`dfxp_CommunicateFixedQnts_Opt` (`dfxpComm.cpp:1018-1027`) to all five
instances. −9.90 dBFS RMS target.

### 8.3 The look-ahead limiter

Ring write and read (`Maxi32.c:296-301`, left; `:390-395`, right):

```c
dly_l_out      = *(s->ptr_l);                       /* oldest = max_delay old */
*(s->ptr_l)    = gain_boost * s->max_output * in1;  /* newest                 */
new_abs_l      = fabsf(*(s->ptr_l));
(s->ptr_l)++;  if (s->ptr_l >= s->dly_start_l + s->max_delay) s->ptr_l = s->dly_start_l;
```

Envelope update — **ramp mode** (`Maxi32.c:304-336`):

```c
if (s->ramp_count_l) {
    if (fabsf(dly_l_out) > s->env_l) s->env_l = fabsf(dly_l_out);       /* :311-315 */
    if (new_abs_l > s->max_abs_l) {                                      /* :318     */
        s->max_abs_l   = new_abs_l;
        s->ramp_count_l = s->max_delay;
        tmp_delta = (new_abs_l - s->env_l) / (s->max_delay + 1);         /* :328     */
        if (tmp_delta > s->delta_l) s->delta_l = tmp_delta;              /* :329-330 */
    } else s->ramp_count_l--;
    s->env_l += s->delta_l;                                              /* :335     */
}
```

Envelope update — **release mode** (`Maxi32.c:337-362`):

```c
else {
    s->env_l = s->env_l * s->release_time_beta + MAXI_ENVELOPE_BIAS;     /* :340     */
    if (fabsf(dly_l_out) > s->env_l) s->env_l = fabsf(dly_l_out);        /* :345-349 */
    if (new_abs_l > s->env_l) {                                          /* :352     */
        s->max_abs_l    = new_abs_l;
        s->delta_l      = (new_abs_l - s->env_l) / (s->max_delay + 1);   /* :358     */
        s->env_l       += s->delta_l;
        s->ramp_count_l = s->max_delay;
    }
}
```

Output (`Maxi32.c:365-386`):

```c
if (s->env_l > s->max_output) out1 = dly_l_out * s->max_output / s->env_l;
else                          out1 = dly_l_out;
```

The envelope is a **linear ramp to the incoming peak, finishing exactly when
that peak arrives at the output tap** — that is the whole point of the
`max_delay` look-ahead. `MAXI_ENVELOPE_BIAS = 1.0e-24` (`c_max.h:48`) prevents
denormals in the release multiply.

### 8.4 Constants

| Parameter | Value | Source |
|---|---|---|
| `max_output` | **0.966051** (−0.30 dBFS ceiling) | `Play32.c:411`, `Maxi32.c:91`; never written by the host |
| `target_level` | **0.32** | `c_max.h:53`, pushed `dfxpComm.cpp:1018-1027` |
| `max_delay` | `(int)(internal_fs × 0.00075)` | `MAXI_LOOK_AHEAD_DELAY 0.00075` (`c_max.h:49`), pushed `dfxpComm.cpp:765-776` |
| `release_time_beta` | see below | `dfxp_MaxCommunicateReleaseTime`, `dfxpComm.cpp:1522-1552` |
| `MAXI_MAX_DELAY_LEN` | 96 (ring capacity per channel) | `c_max.h:47` |
| `quantize_on_flag` | **0** — never written, array zeroed at `ComsftwrCPP.cpp:80-81` | ⇒ the whole dither/requantise block `Maxi32.c:483-582` is dead |
| `wet_gain / dry_gain` | 1.0 / 0.0 | `Play32.c:407-408` ⇒ `kerWetDry` at `:591` is a no-op |

Release: `DSP_PLAY_MAX_RELEASE_TIME_BETA_MIDI = 85` (`c_play.h:66`) indexes an
EXP table over `[MAXIMIZE_MIN_TIME_CONST 0.1, MAXIMIZE_MAX_TIME_CONST 100.0]` ms
(`c_max.h:38-39`, `dfxpQnt.cpp:409-416`), then `qntIToRTimeConstantBeta`
(`dfxpQnt.cpp:426-429`, §4.6):

```
factor = 1000^(1/127) = 1.0558981944
tc[85] = 0.1 × factor⁸⁵ = 10.182959 ms
beta   = exp(−1 / (0.010182959 × fs))
```

| `fs` | `max_delay` | look-ahead | `release_time_beta` |
|---|---|---|---|
| 44100 | **33** | 0.7483 ms | **0.997776** |
| 48000 | **36** | 0.7500 ms | **0.997956** |

> **Validation.** `Play32.c:412-413` / `Maxi32.c:92-93` hard-code
> `max_delay = 33` and `release_time_beta = 0.997776` "from quick pick 1,
> 44.1 kHz". Both recomputed values match exactly.

`MAXI_MAX_DELAY_LEN = 96` limits the usable internal rate to
`96 / 0.00075 = 128 kHz`; at the maximum reachable internal rate of 96 kHz
(§12.1) `max_delay = 72`, so the fixed array is adequate.

### 8.5 Gain-boost mapping — and the MUSIC2 saturation

`dfxp_CommunicateDynamicBoost` (`dfxpComm.cpp:686-779`):

```c
pc_gain_boost = midi_from_registry;
switch (music_mode) {                                          /* :709-717 */
  case MUSIC2:  pc_gain_boost = (int)(1.8 * pc_gain_boost); break;   /* always */
  case SPEECH:  pc_gain_boost = (int)(1.8 * pc_gain_boost); break;
}
if (pc_gain_boost > 127) pc_gain_boost = 127;                  /* :720-721 */

if (bypass_dynamic_boost || bypass_all) pc_gain_boost = 0;     /* :732-733 */

pc_gain_boost = (int)(pc_gain_boost * PLY_OPTIMIZER_BOOST_MAX_SCALE);  /* 0.7, :742 */
qntIToRCalc(dynamic_boost_qnt_hdl, pc_gain_boost, &dsp_gain_boost);    /* :745 */
```

`PLY_OPTIMIZER_BOOST_MAX_SCALE = 0.7` (`c_play.h:90`). The qnt is
`QNT_RESPONSE_MAXI_BOOST_DSP` over `[0, 30]` dB (`dfxpQnt.cpp:154-161`,
`DSP_MAXIMIZE_GAIN_BOOST_MIN/MAX_VALUE`, `c_max.h:41-42`) — the piecewise dB
table of §4.3.

Effective mapping **under MUSIC2** (the shipped mode):

| UI (0…10) | MIDI | ×1.8 (clamped) | ×0.7 (int) | dB | `gain_boost` |
|---|---|---|---|---|---|
| 0 | 0 | 0 | 0 | 0.00 | 1.000000 |
| 1 | 13 | 23 | 16 | 1.60 | 1.202264 |
| 2 | 25 | 45 | 31 | 3.10 | 1.428894 |
| 3 | 38 | 68 | 47 | 4.70 | 1.717908 |
| **4.02 (default)** | **51** | **91** | **63** | **6.60** | **2.137962** |
| 5 | 64 | 115 | 80 | 10.00 | 3.162278 |
| **5.59** | **71** | **127** | **88** | **11.60** | **3.801894** ← saturates here |
| 6…10 | 76…127 | 127 | 88 | 11.60 | 3.801894 |

Without the music-mode factor the ceiling would also be 11.60 dB (`127×0.7 = 88`)
but it would be reached only at UI 10. **MUSIC2 compresses the useful travel of
the Dynamic Boost slider into its lower 56 %.**

Note `gain_boost` is only the *ceiling*; the auto-gain loop backs it off to
`target_level / sqrt_level` whenever the estimated boosted RMS would exceed
0.32.

### 8.6 State and buffers

```c
float dly_start_l[MAXI_MAX_DELAY_LEN];   /* 96 floats */
float dly_start_r[MAXI_MAX_DELAY_LEN];   /* 96 floats */
float *ptr_l, *ptr_r;
float max_abs_l, max_abs_r, delta_l, delta_r, env_l, env_r;
int   ramp_count_l, ramp_count_r;
float noise1_old, noise2_old;            /* dead (dither) */
double level, a0, filt_gain;             /* MUST be f64 */
/* dsp/ptutil/include/c_max.h:100-126 */
```

`DSPS_SOFT_MEM_MAXIMIZER_LENGTH == 0` (`c_dsps.h:82`) — the rings live inside
the parameter block, which is why the struct declares them by value.

Reset on `DSPS_INIT_MEMORY | DSPS_ZERO_MEMORY` (`Maxi32.c:171-176`) zeroes both
rings but **not** `env_l/env_r/delta_l/delta_r/ramp_count_*` — those are only
cleared under `DSPS_INIT_PARAMS` (`Maxi32.c:97-102`). `delta_l`/`delta_r` are
never explicitly initialised at all; they rely on the `calloc` of the parameter
array. Initialise everything in the port.

### 8.7 CPU cost

Per stereo sample: 1 `sqrtf`, 1 division for `gain_boost`, up to 2 divisions for
`delta`, up to 2 divisions for the output normalisation → **up to 5 divisions
and 1 sqrt**, plus ~10 branches. On x86-64 a scalar `divss` is ~11–14 cycles and
`sqrtss` ~12–14. This makes the Maximizer the **second most expensive block**
after the Aural exciter, roughly 20–25 % of the chain.

Cheap wins for the port: hoist `recip_sqrt_level` (one `rsqrtss` + one
Newton step instead of `sqrt` then `div`), and precompute
`1.0 / (max_delay + 1)` once per format change.

### 8.8 Rust sketch

```rust
pub struct Maximizer {
    level: f64, a0: f64, filt_gain: f64,        // f64 is mandatory
    gain_boost: f32, target_level: f32, max_output: f32,
    beta: f32, max_delay: usize, inv_delay_p1: f32,
    ring: [[f32; 96]; 2], idx: [usize; 2],
    env: [f32; 2], delta: [f32; 2], max_abs: [f32; 2], ramp: [i32; 2],
}

const MAXI_TARGET_LEVEL: f32 = 0.32;
const MAXI_MAX_OUTPUT:   f32 = 0.966_051;
const MAXI_LOOK_AHEAD_S: f32 = 0.000_75;
const MAXI_ENV_BIAS:     f32 = 1.0e-24;
const MAXI_GAIN_FLOOR:   f32 = 1.06;

impl Maximizer {
    pub fn prepare(&mut self, fs: f32) {
        let w = 6.283_185 * 0.1 / fs as f64;          // 0.1 Hz
        let c = w.cos();
        self.a0 = 2.0 - c - (c * c - 4.0 * c + 3.0).sqrt();
        self.filt_gain = 1.0 - self.a0;
        self.max_delay = (fs * MAXI_LOOK_AHEAD_S) as usize;   // truncate, not round
        self.inv_delay_p1 = 1.0 / (self.max_delay as f32 + 1.0);
        self.beta = (-1.0f32 / (0.010_182_959 * fs)).exp();
    }
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        self.level = self.level * self.a0 + (l as f64 * l as f64) * self.filt_gain; // LEFT only
        let sl = self.level.sqrt() as f32;
        let g = if self.gain_boost * sl > self.target_level {
            (self.target_level / sl).max(MAXI_GAIN_FLOOR)
        } else { self.gain_boost };
        let k = g * self.max_output;
        (self.limit(0, l * k), self.limit(1, r * k))
    }
    #[inline]
    fn limit(&mut self, c: usize, x: f32) -> f32 {
        let i = self.idx[c];
        let dly_out = self.ring[c][i];
        self.ring[c][i] = x;
        let new_abs = x.abs();
        self.idx[c] = if i + 1 >= self.max_delay { 0 } else { i + 1 };
        if self.ramp[c] != 0 {
            self.env[c] = self.env[c].max(dly_out.abs());
            if new_abs > self.max_abs[c] {
                self.max_abs[c] = new_abs;
                self.ramp[c] = self.max_delay as i32;
                let d = (new_abs - self.env[c]) * self.inv_delay_p1;
                if d > self.delta[c] { self.delta[c] = d; }
            } else { self.ramp[c] -= 1; }
            self.env[c] += self.delta[c];
        } else {
            self.env[c] = self.env[c] * self.beta + MAXI_ENV_BIAS;
            self.env[c] = self.env[c].max(dly_out.abs());
            if new_abs > self.env[c] {
                self.max_abs[c] = new_abs;
                self.delta[c] = (new_abs - self.env[c]) * self.inv_delay_p1;
                self.env[c] += self.delta[c];
                self.ramp[c] = self.max_delay as i32;
            }
        }
        if self.env[c] > self.max_output { dly_out * self.max_output / self.env[c] } else { dly_out }
    }
}
```

---

## 9. Effect 5 — Bass

### 9.1 Algorithm: one parametric peaking biquad, inline in the host

`Play32.c:718-758` (there is no separate DSP module):

```c
/* Adapted from macro kerSosFiltDirectForm2TransPara
 * This special purpose Transformed Direct2 version allows non-unity b0, but
 * assumes b1 == a1, as in parametric boost/cut filters.  Coeffs ordered b0,b1,b2,a2. */
out1        = s->in1_w1 + s->b0 * (in1 + 1.0e-30f);      /* :732 */
s->in1_w1   = (in1 - out1) * s->b1 + s->in1_w2;          /* :733 */
s->in1_w2   = s->b2 * in1;                                /* :734 */
s->in1_w2  -= s->a2 * out1;                               /* :735 */

if (s->stereo_in_flag) { /* same for channel 2 */ }       /* :738-744 */
else out2 = 0.0;                                          /* :754   */
```

This is a transposed Direct-Form-II biquad exploiting `b1 == a1` (true for
`filtCalcParametric`, see `FiltCalcBiqd.cpp:217`) so `b1·x − a1·y` collapses to
`(x − y)·b1` — one multiply saved per sample. Identical topology to the EQ
sections (`09-dsp-eq.md §8`, `SosProcess.cpp:576-578`).

Note `out2 = 0.0` in mono mode (`Play32.c:750-754`) with the modification note
dated 7/28/09: previously `out2 = out1`, which doubled mono bass.

### 9.2 Design constants

```c
/* dsp/ptutil/include/c_play.h:124-127 */
#define DSP_PLY_BASSBOOST_MIN_VALUE   0.0
#define DSP_PLY_BASSBOOST_MAX_VALUE  15.0
#define DSP_PLY_BASSBOOST_CENTER_FREQ 90.0
#define DSP_PLY_BASSBOOST_Q           2.5
```

Two earlier commented-out parameter sets survive at `c_play.h:110-123`
(100 Hz/Q 1.5/12 dB, then 73.4 Hz/Q 2.5/15 dB) — historical only.

The 128-entry biquad array is built once per format change by
`dfxp_InitDynamicQnts_Play` (`dfxpQnt.cpp:514-541`) via `qntIToBoostCutInit`
(§4.7) with `FILT_BOOST_CUT`, and looked up by
`dfxp_CommunicateBassBoost` (`dfxpComm.cpp:871-943`), which writes
`b0,b1,b2,a1,a2` to `DSP_PLAY_B0..A2` (`c_play.h:192-196`) on the front
instance, and additionally on the subwoofer instance when
`num_channels_out > 2` (`dfxpComm.cpp:922-940`).

```
boost_dB(midi) = midi × 15.0 / 127 = midi × 0.118110236
```

### 9.3 The Q-warping traps in `filtCalcParametric`

`dsp/ptutil/Filt/FiltCalcBiqd.cpp:109-222`. Two clamps apply, and `f->Q` is
**mutated in place**:

```c
#define FILT_Q_UPPER_LIMIT_FREQ 60.0
#define FILT_Q_LOWER_LIMIT_FREQ 20.0
#define FILT_Q_UPPER_LIMIT      20.0
#define FILT_Q_LOWER_LIMIT       1.0
#define FILT_Q_LIMIT_SCALE ((20.0-1.0)/(60.0-20.0))        /* 0.475 */
#define FILT_BOOST_WARP_LEVEL    6.0
#define FILT_BOOST_MAX_Q        20.0
#define FILT_BOOST_MIN_Q         0.2
#define FILT_BOOST_SCALE   ((20.0-0.2)/6.0)                /* 3.3   */
```

1. **Low-frequency Q limit** (`:160-166`): only if `center_freq < 60 Hz`. At
   90 Hz it never fires.
2. **Low-boost Q limit** (`:169-176`): if `|boost| < 6 dB`, then
   `maxQ = |boost|·3.3 + 0.2`. This **does** fire for the bottom third of the
   Bass slider:

   | MIDI | boost dB | `maxQ` | effective Q |
   |---|---|---|---|
   | 1 | 0.1181 | 0.590 | **0.590** |
   | 5 | 0.5906 | 2.149 | **2.149** |
   | 13 | 1.5354 | 5.267 | 2.5 |
   | ≥ 13 | ≥ 1.5354 | ≥ 5.267 | 2.5 |

   So the bass peak is **wide and gentle at low settings and narrows to Q 2.5
   from about MIDI 13 (UI 1.0) upward**. This is deliberate — the comment at
   `FiltCalcBiqd.cpp:115-121` explains it prevents "boinks" and precision loss
   at low frequency / high Q.
3. **Zero boost short-circuits** (`:132-137`): `section_on_flag = FALSE`,
   `b0 = 1, b1 = b2 = a1 = a2 = 0` — the difference equation becomes a
   pass-through after one sample. But note the host only *stops calling* the
   bass block when the button is off, and `setEffectValue` turns the button off
   only at exactly `value == 0.0` (`DfxDspPrivate.cpp:295-302`).

### 9.4 Design maths (for exactness)

```c
center_freq = f->r_center_freq / f->r_samp_freq;          /* normalised, fs=1  */
bandwidth   = center_freq / Q;
a   = tan(PI * (center_freq - 0.25));                      /* bilinear warp    */
asq = a*a;
A   = 10^(boost/20);
F   = (|boost| < 6) ? sqrt(A) : (A > 1 ? A/sqrt(2) : A*sqrt(2));
xfmbw  = filtBW2ANGLE(a, bandwidth);                       /* :55-82           */
C      = 1 / tan(2*PI*xfmbw);
alphad = (|A²−F²| <= SPN) ? C : sqrt(C²·(F²−1)/(A²−F²));
alphan = A · alphad;
b0 = (1+asq) + alphan·(1−asq);   b1 = 4a;   b2 = (1+asq) − alphan·(1−asq);
a0 = (1+asq) + alphad·(1−asq);               a2 = (1+asq) − alphad·(1−asq);
/* normalise by a0; then a1 := b1 */
```

`filtBW2ANGLE` (`FiltCalcBiqd.cpp:55-82`) with its two hacks:

```c
theta = 0.5 * (PI - asin(d) - delta);
tmp   = 0.5 * (asin(d) - delta);
if (tmp > 0.0 && tmp < theta) theta = tmp;     /* principal branch, :74-75 */
if (bandwidth >= 0.5) theta = 0.005;           /* hi-freq hi-Q hack, :78-79 */
return theta / (2*PI);
```

Neither hack fires for 90 Hz / Q 2.5 (bandwidth ≈ 8.16 × 10⁻⁴), but both must be
present for the shared EQ path — see `09-dsp-eq.md §7`.

### 9.5 Golden coefficients

Recomputed from the formulas above; verified by evaluating `|H(e^{jω})|` at
90 Hz, which reproduces the requested boost to 3 decimal places at every point.

**fs = 44100:**

| MIDI | boost dB | Q_eff | `b0` | `b1` = `a1` | `b2` | `a2` | \|H(90 Hz)\| |
|---|---|---|---|---|---|---|---|
| 0 | 0.0000 | — | +1.00000000 | +0.00000000 | +0.00000000 | +0.00000000 | 0.000 dB |
| 1 | 0.1181 | 0.590 | +1.00014625 | −1.97847220 | +0.97848861 | +0.97863486 | +0.118 dB |
| 13 | 1.5354 | 2.500 | +1.00045287 | −1.99515171 | +0.99486287 | +0.99531574 | +1.535 dB |
| 26 | 3.0709 | 2.500 | +1.00090947 | −1.99554708 | +0.99480168 | +0.99571115 | +3.071 dB |
| 51 | 6.0236 | 2.500 | +1.00181203 | −1.99621433 | +0.99456643 | +0.99637846 | +6.024 dB |
| **68** | **8.0315** | 2.500 | **+1.00322233** | **−1.99559883** | **+0.99254058** | **+0.99576291** | **+8.031 dB** |
| 89 | 10.5118 | 2.500 | +1.00546197 | −1.99519579 | +0.98989786 | +0.99535983 | +10.512 dB |
| 127 | 15.0000 | 2.500 | +1.01144758 | −1.99488398 | +0.98360042 | +0.99504800 | +15.000 dB |

**fs = 48000:**

| MIDI | boost dB | `b0` | `b1` = `a1` | `b2` | `a2` |
|---|---|---|---|---|---|
| 13 | 1.5354 | +1.00041616 | −1.99555703 | +0.99527937 | +0.99569552 |
| 51 | 6.0236 | +1.00166505 | −1.99653366 | +0.99500717 | +0.99667222 |
| **68** | **8.0315** | **+1.00296103** | **−1.99596798** | **+0.99314548** | **+0.99610650** |
| 127 | 15.0000 | +1.01051958 | −1.99531096 | +0.98492986 | +0.99544943 |

### 9.6 State and cost

State: 4 floats (`in1_w1, in1_w2, in2_w1, in2_w2`, `c_play.h:308-311`), zeroed
at `Play32.c:222-225`. Cost: 2 × (3 mul + 3 add) ≈ 12 flops per stereo sample —
under 5 % of the chain.

### 9.7 A documented inconsistency

`dsp/ptutil/include/dfxpDefs.h:210-211`:

```c
/* DEFAULT DB VALUE FOR BAND1 - IT MUST MATCH DEFAULT FOR BASS BOOST */
#define DFXP_DEFAULT_BAND1_DB_VAL  5.35
```

But the Bass default is MIDI 68 → **8.0315 dB**, not 5.35 dB. 5.35 is
numerically the default *UI* value (68 / 12.7 = 5.354). The comment appears to
conflate the UI value with a dB value. Do not propagate the confusion: EQ band 1
default is 5.35 dB (see `09-dsp-eq.md`), Bass default is 8.03 dB at 90 Hz.

---

## 10. Dead modules that still live inside `ply0`

| Module | Where | Why it is dead |
|---|---|---|
| **Vocal eliminator** | `Play32.c:442-637` plus 15 hard-coded 8th-order band-stop/band-pass filters at `Play32.c:885-1204` | `dfxpGetButtonValue(DFX_UI_BUTTON_VOCAL_REDUCTION_ON)` returns `IS_FALSE` unconditionally (`dfxpGet.cpp:155-159`). All filter coefficients are hard-coded for 44100 Hz only. |
| **Dly8 headphone ambience** | `Play32.c:763-860` calling `dspsDly8Process32` (`dsp/ptechDsp/Dly32/Dly832/dly8p32.c`) | `DSP_PLAY_HEADPHONE_ON` is written as literal `0` to every instance (`dfxpComm.cpp:498-509`); the older headphone tech was replaced by `BinauralSyn`. Its 8 delays / feedbacks / pans are still pushed every communicate by `dfxp_CommunicateFixedQnts_Delay` (`dfxpComm.cpp:1158-1243`) using `PLY_HEADPHONE_DELAY0..7` (29.0/32.7/13.0/21.8/20.6/16.7/36.0/27.2 ms × `PLY_DELAY_FACTOR 0.75`) and `PLY_HEADPHONE_PAN_SETTING0..7` (`c_play.h:137-177`), wasting 262144 floats (`DSPS_SOFT_MEM_DELAY_LENGTH`, `c_dsps.h:70`) = **1 MiB**. |
| **Cross-feed** | `Play32.c:786-845` | Guarded by `#ifdef PLY_DO_CROSS_FEED`, never defined. |
| **PEQ** | `dsp/ptechDsp/Peq/Peq832/peq8p32.c` | Compiled, but `ply0` never calls it; the EQ is the separate `GraphicEq`/`SOS` module. |
| **Maximizer dither/requantise** | `Maxi32.c:483-582` | `quantize_on_flag` is never written and the parameter array is zeroed (`ComsftwrCPP.cpp:80-81`). |
| **Lex modulation oscillator** | `Lex32.c:117-144` builds it, `:399-419` etc. read it | All reads are inside `#if (PT_DSP_BUILD == PT_DSP_DSPFX)`. |
| **Wide dispersion delays** | `Wide32.c:123-129` | Allocated, never read by the 2025 process function. |

**Port decision.** Drop all of the above. That reclaims 1 MiB (Dly8) + 32 KB
(Lex tables) + 28 KB (Wide) of the 1.5624 MiB `DSPS_SOFT_MEM_PLAY_LENGTH`
(`c_dsps.h:96` → 409598 floats) per `ply0` instance.

---

## 11. Memory, state and reset

### 11.1 Total DSP memory per `ply0` instance

```
DSPS_SOFT_MEM_PLAY_LENGTH = AURAL(0) + MAXIMIZER(0) + LEX + WIDE + DELAY
                          = 0 + 0 + 140226 + 7228 + 262144
                          = 409598 floats = 1.5625 MiB      (c_dsps.h:96)
```

allocated once by `comSftwrAllocDspMem` with `malloc`
(`Comsftwr.c:846`) and carved up by `Play32.c:278-393`:

```
offset 0                          Aural   (length 0)
offset 0                          Lex     (140226)
offset 140226                     Wide    (7228)
offset 147454                     Dly8    (262144)     ← dead
offset 409598                     Maximizer (length 0)
```

Plus `dsp_params` (`DSPFX_MAX_NUM_PROCS × 2 × 128` floats) and `dsp_state`
(`DSPFX_MAX_NUM_PROCS × 64` floats), both statically embedded in
`comSftwrHdlType` (`u_comSftwr.h:42-43`).

A Rust port sized for a 48 kHz stereo endpoint needs:

```
Lex ring        41404 f32   162 KiB
Maximizer       2×96  f32   0.75 KiB
Aural            8    f32
Bass             4    f32
Wide             0
                            ≈ 163 KiB total
```

### 11.2 Per-sample state that must be reset on stream start / device change

| Effect | State | Reset site in C |
|---|---|---|
| Aural | `out{1,2}_minus{1,2}`, `in{1,2}_minus{1,2}` (8 f32) | `Auralp32.c:118-129` |
| Lex | whole ring, `old_bandwidth_val_l`, `old_damp_val{1,2}_l`, `D4_out`, `ptr` | `Lex32.c:156-162` (ring only; scalars under `DSPS_INIT_PARAMS` at `:109-112`) |
| Wide | none used | — |
| Bass | `in{1,2}_w{1,2}` (4 f32) | `Play32.c:222-225` |
| Maximizer | both rings; `env`, `delta`, `max_abs`, `ramp_count`, `level` | rings `Maxi32.c:171-176`; scalars only under `DSPS_INIT_PARAMS` `Maxi32.c:97-117` |

`comSoftDspZeroMemory` (`Com.cpp:363-376`) drives `DSPS_ZERO_MEMORY`, which
clears the *signal* memory but **not** the Maximizer envelope/level state. On a
device switch the old envelope and the old 1.6-second RMS estimate survive.
The Rust port should reset everything in `prepare()`.

### 11.3 Sample-rate handling

`dfxpBeginProcess` (`dsp/ptutil/dfxp/dfxpProcess.cpp:46-151`):

```c
internal_sampling_freq = r_sample_rate;  internal_rate_ratio = 1;      /* :97,100 */
if (r_sample_rate > DFXP_MAX_INTERNAL_SAMPLING_FREQ /* 48000 */) {
    if (r_sample_rate < DFXP_MAX_SAMPLING_FREQ /* 192000 */) {
        internal_sampling_freq = r_sample_rate / 2.0;  internal_rate_ratio = 2;  /* :108-110 */
    } else {
        internal_sampling_freq = r_sample_rate / 4.0;  internal_rate_ratio = 4;  /* :114-116 */
    }
}
```

Supported range `[DFXP_MIN_SAMPLING_FREQ 16000, DFXP_MAX_SAMPLING_FREQ 192000]`,
`≤ 8` channels, `8…32` bits (`u_dfxp.h:43-52`, checked `dfxpProcess.cpp:121-132`).

Decimation/interpolation is **sample-and-hold with no anti-alias filter**
(`Comwave.cpp:124-146` and the matching upsample). Every effect therefore
operates at 44.1/48 kHz for the common rates, at `fs/2` for 88.2/96/176.4 kHz,
and at 48 kHz for 192 kHz.

Everything that depends on the rate is recomputed in
`dfxp_InitDynamicQnts` (`dfxpQnt.cpp:196-220`) before `dfxp_ComLoadAndRun`
(`dfxpProcess.cpp:135-142`), so a rate change rebuilds all six qnt tables and
re-runs every `*_INIT` with `DSPS_INIT_MEMORY | DSPS_INIT_PARAMS`
(`Com.cpp:349`).

---

## 12. CPU cost summary

Static instruction counts per **stereo sample** at 48 kHz, from the sources
above. These are counts, not measurements — profile before optimising.

| Block | mul | add/sub | div | sqrt | transcendental | mem ops | share (est.) |
|---|---|---|---|---|---|---|---|
| Aural (Fidelity) | 14 | 12 | 0 | 0 | **2 × `sin`** | 16 (all L1) | **~45 %** |
| Lex (Ambience) | ~26 | ~40 | 0 | 0 | 0 | ~40 (162 KiB ring, L2) | ~28 % |
| Wide (Surround) | 5 | 5 | 0 | 0 | 0 | 0 | ~3 % |
| Bass | 6 | 6 | 0 | 0 | 0 | 0 | ~4 % |
| Maximizer (DynBoost) | ~12 | ~10 | **≤ 5** | **1** | 0 | 8 (L1) | ~20 % |

Total for the full chain on one stereo pair: roughly 60–70 multiplies,
70–80 adds, 5 divides, 1 sqrt, 2 sines and ~65 memory touches per sample.
At 48 kHz that is well under 1 % of a modern core — but at 192 kHz with four
`ply0` instances (7.1) it is 20× that. Keep the per-instance cost honest.

Biggest wins for the Rust port, in order:

1. **Vectorise / polynomialise the Aural sine.** A 4-wide SSE/AVX range-reduced
   sine over a 4-sample block roughly halves the total chain cost.
2. **Process in blocks, not sample-at-a-time**, so coefficients stay in
   registers. The C code re-reads `s->aural_drive` etc. every sample through a
   `volatile`-flavoured struct pointer.
3. **`rsqrt` + Newton** in the Maximizer instead of `sqrt` then `div`.
4. **Shrink the Lex ring** to the actual rate; consider splitting it into
   per-stage rings so each stage's working set is contiguous (better prefetch),
   at the cost of losing the elegant net-+1 trick.

---

## 13. Windows machinery → Linux / PipeWire equivalents

| Windows mechanism | What it achieves | Linux / PipeWire equivalent |
|---|---|---|
| Registry `HKCU\SOFTWARE\…\valFidelity` etc. holding the MIDI 0…127 knob value; re-read on **every** `dfxpCommunicate*` (`dfxpGet.cpp:69-140`, `dfxpSession.cpp`) | Cross-process parameter transport between the tray UI and the audio DLL | Single-process app: hold parameters in an `ArcSwap<EffectParams>` or a triple-buffer written by the UI thread, read once per PipeWire `on_process` callback. Persist to `$XDG_CONFIG_HOME/fxsound/state.toml` on change (debounced), never from the RT thread. |
| `dfxpCommunicateAllNonFixed(..., i_communicate_slowly_flag)` round-robining one effect per buffer to amortise registry reads (`dfxpComm.cpp:206-307`) | Cost control for the registry polling | Unnecessary — drop it. Recompute coefficients only when the parameter generation counter changes. |
| `DfxDspPrivate::processTimer()` called at the top of every `processAudio` (`DfxDspPrivate.cpp:157-189`) | Pulls registry changes into the DSP | Replace with an atomic `param_epoch` compare in `on_process`. |
| `comSftwrWriteParam` — unsynchronised float store into a shared array read by the audio thread (`Comsftwr.c:69-94`) | Lock-free-by-accident parameter update | A real lock-free snapshot. Torn reads of the Lex `wet/dry` pair or the bass `b0..a2` quintuple are audible as clicks. |
| `malloc`/`realloc` of 1.5 MiB DSP memory on format change (`Comsftwr.c:846`, `Comsftwr.c:885`) | Ring allocation | Allocate in `prepare(sample_rate, channels)` on the PipeWire *main* thread (in the `param_changed` handler before `Ready`), never inside `on_process`. Pre-allocate for the maximum supported rate if you want to avoid reallocation entirely. |
| `QueryPerformanceCounter` seeding the demo-mode counter (`Comsftwr.c:327`) | Demo timing | Dead code (`PT_DSP_DFX`). Drop. |
| Multi-instance `ply0` for front/rear/side/center/subwoofer with Windows WAVEFORMATEXTENSIBLE channel ordering and the de-interleave/re-interleave shuffle (`dfxpProcessReal.cpp:230-339, 514-572`) | Windows multichannel endpoints | PipeWire gives you a `spa_audio_info_raw` with an explicit `position[]` channel map. Match on `SPA_AUDIO_CHANNEL_FL/FR/FC/LFE/RL/RR/SL/SR` and build the pair routing from that instead of hard-coding Microsoft's order. For a v1 stereo-only filter node, implement the **front** instance only. |
| `DfxDsp::processAudio(short int*, short int*, …)` taking `short*` that actually carries 8/16/20/24/32-bit ints (`dfxpProcessInt.cpp:42-45`) | Format-agnostic buffer handover | Negotiate `SPA_AUDIO_FORMAT_F32` (or `F32P`) in `EnumFormat` and skip the int↔float conversion entirely. If a device forces S16/S24, convert in the port's own format adapter. |
| Virtual audio driver (FxSound Audio Enhancer) as the system-wide capture point | System-wide processing | A PipeWire **filter-chain / `pw_filter`** node with `media.class = Audio/Sink` plus a loopback to the real device, or a `filter-chain` module instantiated from a config; set `node.passive = true` and `node.latency = <quantum>/<rate>`. The look-ahead adds `max_delay` (33–36 samples) of latency — report it via `SPA_PARAM_Latency`. |
| `DSP_DENORM_BIAS 1.0e-36` and `+1.0e-30f` biases in every recursive filter (`boardrv1.h:115`, `Auralp32.c:238`, `Lex32.c:323-324`, `Play32.c:732`) | Avoid x87/SSE denormal stalls | Keep the biases **and** set FTZ/DAZ. In Rust: `std::arch::x86_64::_MM_SET_FLUSH_ZERO_MODE(_MM_FLUSH_ZERO_ON)` + `_MM_SET_DENORMALS_ZERO_MODE(_MM_DENORMALS_ZERO_ON)` once on the RT thread (aarch64 sets FZ in FPCR). |
| `realSampleForceLegalValues_ArrayOnly` guarding against NaN/Inf from the host (`dfxpProcessReal.cpp:355`, skipped in "lean and mean" mode) | Input sanitising | PipeWire can hand you garbage from a misbehaving client. Keep a cheap `is_finite` sweep, or rely on the limiter's `max_output` clamp plus a final `clamp(-1.0, 1.0)`. |

---

## 14. Port-behaviour decisions you must make explicitly

These are places where "faithful" and "correct" diverge. Decide each one and
record it.

1. **`getEffectValue` returns 0…1, `setEffectValue` takes 0…10.** (§3.1) —
   recommend: normalise to `0.0..=1.0` everywhere, note in the changelog.
2. **MUSIC2 is hard-wired and rescales Ambience (×0.34) and Dynamic Boost
   (×1.8).** (§3.4) — recommend: **keep** the factors so presets sound the same,
   but expose the true effective range in the UI, or remap the slider so its
   full travel is useful. Currently Ambience's bottom 30 % and Dynamic Boost's
   top 44 % do nothing.
3. **Ambience `wet_gain` goes negative** for `12 > eff_midi ≥ 0` (§6.7,
   `dfxpComm.cpp:630`: `(pc_liveliness − 12) × …`). It is masked because the
   bypass flag uses the *unscaled* MIDI value, so the block is skipped
   below UI 0.95 — but between UI 0.95 and 3.07 the reverb runs with
   `wet_gain ≤ 0`, i.e. a phase-inverted (and at UI ≤ 0.94 also bypassed) tail.
   Recommend: clamp `wet_gain` to `≥ 0`.
4. **Ambience `dry_gain` exceeds 1.0** below eff_midi 12 (up to 1.044 at 0,
   `dfxpComm.cpp:631`). Same masking, same recommendation: clamp to `≤ 1.0`.
5. **Maximizer level estimate uses the left channel only** (`Maxi32.c:258-259`).
   Recommend: use `max(|L|,|R|)²` or `(L²+R²)/2` and document the change; the
   current behaviour mis-tracks hard-panned material.
6. **`s->level` must be `f64`.** (§8.2) Using `f32` with a pole of 0.99998575
   silently freezes the estimator.
7. **The reverb has no modulation in this build.** (§1.2) If you re-enable it
   you are shipping a different reverb. Recommend: keep it off for v1, keep the
   parameters reserved.
8. **Bass low-boost Q warping** (§9.3) changes the filter *shape*, not just its
   gain, across the bottom of the slider. Port it — removing it makes quiet Bass
   settings sound peaky.
9. **Decimation above 48 kHz is unfiltered sample-and-hold** (§11.3). Recommend:
   either run the effects at the native rate (the Lex ring and the qnt tables all
   scale) or add a proper half-band filter. The current behaviour aliases.
10. **`176400 Hz` maps to an internal rate of `88200 Hz`**, which violates
    `DAW_MAX_INTERNAL_SAMPLING_FREQ 48000` (`dfxpProcess.cpp:104-117` only tests
    `< 192000`). Memory is still adequate because everything is sized for
    96 kHz, but it is clearly unintended. Recommend: clamp the internal rate to
    48 kHz, or drop the decimation entirely per (9).
11. **`quantize_on_flag` never set** — the 16-bit noise-shaped dither in
    `Maxi32.c:483-582` never runs. Do not resurrect it; the Linux sink is float.

---

## 15. Golden test vectors

Use these to validate the Rust port against the derivations above. All values
were produced by re-executing the cited C formulas.

### 15.1 Coefficient checks (must match to ≥ 5 significant figures)

| Quantity | fs | Expected | Cross-check in source |
|---|---|---|---|
| Aural HP `gain, a1, a0` @ MIDI 53 | 44100 | 0.839410, 1.652862, −0.704777 | — |
| Aural HP `gain, a1, a0` @ MIDI 53 | 48000 | 0.851343, 1.680463, −0.724908 | — |
| Aural corner frequency | any | 1745.499 Hz | `500 × 20^(53/127)` |
| Aural harmonic gain | any | 0.5669295 | `0.377953 × 1.5` (`Play32.c:267,269`) |
| Lex `bandwidth`, `1−bandwidth` | 44100 | 0.350108, 0.649892 | **`Play32.c:310-311`: 0.350110, 0.649890** ✔ |
| Lex `damping`, `1−damping` | 44100 | 0.408288, 0.591712 | **`Play32.c:308-309`: 0.408290, 0.591710** ✔ |
| Lex `roomsize` | any | 1.0039370078740 | `Play32.c:312`: 1.0 (approx) |
| Lex `MasterLen` | 44100 | 38040 samples (862.59 ms) | `c_lex.h:66-69`: "859.805 ms" ✔ |
| Lex `MasterLen` | 48000 | 41404 samples (862.58 ms) | — |
| Maxi `max_delay` | 44100 | 33 | **`Play32.c:412`: 33** ✔ |
| Maxi `max_delay` | 48000 | 36 | — |
| Maxi `release_time_beta` | 44100 | 0.997776 | **`Play32.c:413`: 0.997776** ✔ |
| Maxi `release_time_beta` | 48000 | 0.997956 | — |
| Maxi level filter `a0` | 44100 | 0.9999857525 | — |
| Maxi level filter τ | any | 1.5915 s | — |
| Bass biquad @ MIDI 68 | 44100 | see §9.5 | `|H(90 Hz)| = +8.031 dB` |

### 15.2 End-to-end unit checks

1. **Aural is unity on DC.** Feed a DC step of 0.5 with Fidelity at any value;
   after the high-pass settles, output → 0.5 exactly (the HP kills DC, so
   `sin(0) = 0`).
2. **Aural harmonic ceiling.** Feed a 4 kHz sine at 0 dBFS with Fidelity = 10.
   The added term never exceeds ±0.5669295 in magnitude.
3. **Surround is unity at 0.** With Surround = 0, `side = 1.0`, `mid = 1.0`,
   so `L' = L`, `R' = R` bit-exactly.
4. **Surround on a mono source at 10** produces `L' = R' = M × 0.79` — a flat
   −2.05 dB.
5. **Bass at 0** — `section_on_flag == false`, so with the button off the block
   is skipped entirely; with `b0=1, b1=b2=a1=a2=0` it is a one-sample-delayed
   pass-through (`out = w1 + in`, `w1 → 0`).
6. **Maximizer ceiling.** Feed white noise at 0 dBFS with Dynamic Boost = 10.
   Peak output must never exceed `max_output = 0.966051` (−0.30 dBFS).
7. **Maximizer look-ahead latency.** An isolated impulse appears at the output
   exactly `max_delay` samples late (33 @ 44.1 k, 36 @ 48 k).
8. **Maximizer auto-gain settling.** Step from silence to a −20 dBFS RMS sine.
   `sqrt_level` reaches 63 % of final in 1.5915 s ± 1 %.
9. **Ambience bypass boundary.** UI 0.94 (MIDI 11) → block not called;
   UI 0.95 (MIDI 12) → still bypassed (`≤ 12`); UI 1.03 (MIDI 13) → called.
10. **Lex ring invariant.** After N samples the cursor has advanced exactly
    `N mod MasterLen`. Assert this in a debug build — it is the easiest way to
    catch a mis-transcribed stage length.
11. **Chain order.** With Fidelity = 0, Ambience = 0, Surround = 0, Bass = 0 and
    Dynamic Boost = 0, the chain is `out = in × max_output = in × 0.966051`
    (the Maximizer is never bypassed, `Play32.c:867-876`). **This is the single
    most important regression test** — a "all effects off" signal is *not*
    unity, it is −0.30 dB, and a port that returns exactly `in` has got the
    Maximizer routing wrong.

---

## 16. Proposed Rust module layout

```
crates/fxsound-dsp/
├── src/
│   ├── lib.rs             // EffectChain, Params, prepare(), process_block()
│   ├── params.rs          // 0..=1 f32 values, MIDI quantisation, music-mode factors
│   ├── qnt.rs             // the six curve generators of §4, const-evaluated where possible
│   ├── biquad.rs          // filtCalcParametric + filtBW2ANGLE + Butterworth designers
│   ├── fidelity.rs        // §5
│   ├── ambience.rs        // §6  (owns the ring Vec<f32>)
│   ├── surround.rs        // §7
│   ├── dynamic_boost.rs   // §8
│   └── bass.rs            // §9
└── tests/
    └── golden.rs          // §15 vectors
```

```rust
pub struct EffectChain {
    fidelity: Fidelity, ambience: Lex, surround: Surround,
    bass: Bass, dynamic_boost: Maximizer,
    on: EnabledMask,
}

impl EffectChain {
    /// Control thread only. Allocates.
    pub fn prepare(&mut self, sample_rate: f32, max_block: usize) { /* … */ }

    /// RT thread. No allocation, no locks, no syscalls.
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.on.fidelity      { self.fidelity.process(l, r); }
        if self.on.ambience      { self.ambience.process(l, r); }
        if self.on.surround      { self.surround.process(l, r); }
        if self.on.bass          { self.bass.process(l, r); }
        self.dynamic_boost.process(l, r);          // never bypassed — see §15.2(11)
    }
}
```

Enable flags follow the C rule exactly: a block is enabled iff its value is
non-zero (`DfxDspPrivate.cpp:295-302`) **and** the user has not explicitly
bypassed it. The Maximizer is unconditional; "off" is expressed as
`gain_boost = 1.0`.

---

## Open questions / risks for the Rust port

1. **Is the −0.30 dB `max_output` floor acceptable on Linux?** Every sample that
   passes through FxSound is attenuated by 0.966051, even with all five effects
   at zero (§15.2 item 11). On Windows this is masked by the virtual driver
   being the only path; as a PipeWire filter node users will A/B it against
   bypass and notice. Options: keep it (faithful), or make it 1.0 when
   `gain_boost == 1.0` (changes the limiter's behaviour at unity). **Needs a
   product decision.**

2. **Do we keep the MUSIC2 rescaling?** (§3.4, §14.2) Keeping it preserves
   preset compatibility with `.fac` files; dropping it makes both sliders
   usable across their full travel but changes how every existing preset sounds.
   There is no way to have both. The preset format spec (`11-preset-format.md`)
   stores MIDI values, so a migration is possible but lossy.

3. **Aural sine approximation tolerance.** How close does `fast_sin` need to be?
   The argument range is `[−3.393, +3.393]`. A 7th-order minimax gives ~−90 dB
   error; a 9th-order ~−120 dB. The generated harmonics themselves are ~−20 dBFS,
   so −90 dB relative error is −110 dBFS absolute, which is inaudible — but it
   will fail a bit-exactness comparison against the C reference. **Decide
   whether the acceptance test is bit-exactness or a spectral null at −100 dB.**

4. **Lex ring truncation sensitivity.** Every one of the 13 stage lengths is a
   C `(long)` truncation of a float product. Rust's `as usize` truncates the
   same way, but only if the intermediate is computed in the same precision:
   `LAT5_LEFT_DELAY_LEN_NOMINAL * r_roomsize` is `double × float → double` in C
   (`Lex32.c:195`). If Rust computes it in `f32` the result can land on the other
   side of an integer boundary. **Recommend computing all ring lengths in `f64`
   and asserting the §15.1 totals.**

5. **Does anything depend on the Maximizer's un-reset envelope across device
   changes?** (§11.2) The C code deliberately keeps `level` and `env` through
   `DSPS_ZERO_MEMORY`. Resetting them (which is what a clean `prepare()` does)
   means a ~1.6 s auto-gain re-settle on every device switch. That may be more
   noticeable than the stale-state glitch it avoids. **Needs listening tests.**

6. **Channel-pair routing for surround.** The five-instance model (§2.1) is
   built around Windows' fixed channel order. PipeWire's `position[]` map is
   richer (it can express side-vs-back, and arbitrary orders). Is a v1
   stereo-only filter acceptable, or must 5.1/7.1 ship at the same time? The
   routing table's asymmetries (Bass only on front + LFE, Wide off on centre/LFE)
   are easy to port but need a channel-map abstraction that does not exist yet.

7. **Parameter smoothing.** The C code steps every coefficient instantly. The
   audible ones are Aural `drive`, Lex `wet/dry`, Wide `side/mid`, Maxi
   `gain_boost` and the Bass biquad quintuple. Bass in particular re-writes five
   coefficients non-atomically (`dfxpComm.cpp:906-920`) — a torn update mid-block
   is a genuine click source. **Recommend a 10–20 ms linear ramp on gains and a
   full-coefficient-set swap with a short crossfade for the biquad, and verify
   no one relies on instantaneous response.**

8. **Denormal handling on aarch64.** The `1.0e-30`/`1.0e-36` biases were tuned
   for x87/SSE. On ARM with FZ set they are harmless but also unnecessary; on
   ARM *without* FZ the Lex ring can still denormal during long silences.
   **Decide whether to set FPCR.FZ unconditionally on the RT thread** (it changes
   arithmetic globally for that thread, including anything else running on it).

9. **What is `internal_rate_ratio` supposed to do at 176.4 kHz?** (§14.10) The
   code produces an 88.2 kHz internal rate that exceeds its own documented
   maximum. No test coverage exists. **Confirm with a real 176.4 kHz device
   before deciding whether to clamp or to drop decimation altogether.**

10. **Preset round-tripping of effect values.** `loadPreset` populates only the
    cached `.value` fields — the `setEffectValue` calls that would push them to
    the DSP are commented out (`dsp/DfxDspPreset.cpp:86-90`), and the GUI papers
    over it with an explicit re-push loop (`FxController.cpp:1085-1090`). A Rust
    port that makes `load_preset` actually apply the values will change
    behaviour in the window between load and the next UI tick. **Confirm nothing
    (e.g. the "preset modified" dirty flag) depends on the current two-step
    behaviour.**
