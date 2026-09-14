# 09 — Equalizer DSP and the shared filter / quantisation maths

Reverse-engineering spec for the FxSound graphic equaliser, written so a Rust 1.98.1 /
egui-eframe 0.36.0 / PipeWire port can reproduce the audio behaviour sample-for-sample.

Every constant below is cited as `path:line` against the tree at
`/home/blackixxce/Загрузки/fxsound-app-main`. Numbers marked **(computed)** were produced by
re-running the original C code verbatim in `float` precision; the derivation is given so they can
be re-checked.

---

## 0. Scope and file map

| Concern | File |
|---|---|
| Public EQ surface on `DfxDspPrivate` (band gain/freq getters+setters, on/off, preset band-count interpolation) | `dsp/DfxDspEq.cpp` |
| Biquad coefficient design (parametric + shelf), bandwidth→prototype-bandedge map | `dsp/ptutil/Filt/FiltCalcBiqd.cpp` |
| Thin wrappers that design a filter and push it into an SOS slot | `dsp/ptutil/Filt/FiltbiqdSos.cpp` |
| 1st/2nd-order Butterworth LP/HP designers | `dsp/ptutil/Filt/Fil12But.cpp` |
| 1st/2nd-order LP/HP per-sample run functions | `dsp/ptutil/Filt/FiltRun.cpp` |
| Polynomial / biquad frequency-response evaluation (for the UI curve) | `dsp/ptutil/Filt/Filtpoly.cpp` |
| FIR frequency-response evaluation | `dsp/ptutil/Filt/FiltCalcFilterResponse.cpp` |
| Filter struct + `realtype`/`biqdRealtype` aliases | `dsp/ptutil/include/filt.h` |
| Control-value quantisation tables (int↔real↔long, and int→biquad) | `dsp/ptutil/Qnt/*.cpp`, `dsp/ptutil/Qnt/u_qnt.h`, `dsp/ptutil/include/qnt.h` |
| Band layout, Q derivation, master gain / balance / normalisation / volume-levelling entry points | `dsp/ptutil/DspUtil/GraphicEq/*.cpp`, `dsp/ptutil/include/GraphicEq.h` |
| Cascaded second-order-section container + the actual per-sample inner loop | `dsp/ptutil/SOS/*.cpp`, `dsp/ptutil/SOS/u_sos.h`, `dsp/ptutil/include/sos.h` |
| Where the EQ is invoked in the realtime callback | `dsp/ptutil/dfxp/dfxpProcessReal.cpp:141-170` |
| Registry-backed per-band persistence | `dsp/ptutil/dfxp/dfxpEq.cpp` |
| Decimal "snap" helpers used by the Qnt module | `audiopassthru/src/MTH/MthUtil.cpp` (declared in `dsp/ptutil/include/mth.h:40-42`) |

Note: `MthUtil.cpp` is **not** in `dsp/DfxDsp.vcxproj` (checked: no `MTH` entry in that file). `DfxDsp`
is a static lib that leaves `mthCalcQuantDelta` / `mthCalcRoundedValue` / `mthCalcClosestNiceValue`
unresolved until the final app link pulls them from the `audiopassthru` project. In Rust just put
them in one crate.

---

## 1. Numeric precision — read this first

```
codedefs.h:150   #define realtype float
filt.h:24        #define biqdRealtype realtype      /* "For precision of sos coeff calculations" */
codedefs.h:153   #define respDouble realtype
```

**The entire biquad design path is `f32`, not `f64.`** `biqdRealtype` looks like a precision knob
but is wired straight to `float`. Every `tan`, `asin`, `atan2`, `sqrt`, `pow` call in
`filtCalcParametric` is a `double` libm call whose result is immediately truncated to `f32`, and
intermediate products (`a2`, `a4`, `sn*sn + cs*cs`, `C*C`) are accumulated in `f32`.

Consequences you must replicate:

* `a = tan(PI*(f/fs - 0.25))` loses ~7 significant digits. At 20 Hz / 48 kHz, `a ≈ -0.99869` and
  `1 - a*a ≈ 2.6e-3` — a catastrophic cancellation carried out in `f32`. This is exactly the
  "low frequency accuracy problems" the source comment at `FiltCalcBiqd.cpp:140-142` is
  apologising for, and it is the reason the low-frequency Q limiter exists.
* If you compute the coefficients in `f64` in Rust you will get a *better* filter that does not
  match the original. Decide deliberately (see §17).

Everything **except** the coefficient design is also `f32`:
* section states, sample data, master gain, balance, normalisation — all `realtype` (`u_sos.h:47-116`).
* `filtCalcFirResponse` is the one function that is genuinely `double` (`FiltCalcFilterResponse.cpp:30`).
* The band-frequency and Q *derivation* in `GraphicEqReSetAllBandFreqs` is done in `double` and then
  stored into `f32` fields (`GraphicEqSet.cpp:366, 495-518`).

Constants:

| Symbol | Value | Cite |
|---|---|---|
| `PI` (filt) | `3.141592653589793238462643` | `FiltCalcBiqd.cpp:28` |
| `ROOT2O2` | `0.7071067811965475244` (note: last digits are wrong, true √2/2 = 0.70710678118654752**44**) | `FiltCalcBiqd.cpp:29` |
| `SPN` "smallest positive number" | `1.65436e-24` | `FiltCalcBiqd.cpp:30` |
| `MTH_PI` | `3.1415926536` | `mth.h:18` |
| `MTH_TWO_PI` | `6.2831853072` | `mth.h:19` |
| `MTH_FOUR_PI` | `12.5663706144` | `mth.h:20` |
| `kPi` (SOS leveling) | `3.14159265358979323846f` | `SosProcess.cpp:76` |

`ROOT2O2` is a genuine typo in the original (`...11965475244` vs the correct `...11865475244`),
error ≈ 1e-11, irrelevant in `f32`. Reproduce it or not; it cannot change an `f32` result.

---

## 2. Where the EQ sits in the realtime chain

```
dfxpModifyRealtypeSamples()                       dfxpProcessReal.cpp
  ├── buffer-length bookkeeping (registry writes!)  :95-103
  ├── if num_sample_sets > DAW_MAX_BUFFER_SIZE -> return unprocessed   :109-110
  ├── read DFX_UI_BUTTON_BYPASS                     :126
  ├── if !bypass && eq_on && ch in {1,2,6,8}
  │      GraphicEqProcess(...)                      :152-155   <-- THIS SPEC
  ├── else if bypass && eq_on
  │      GraphicEqProcess_MasterGainOnly(...)        :165-168
  └── BinauralSyn / SurroundSyn ...                 :176+
```

`GraphicEqProcess` (`GraphicEqProcess.cpp:33-71`):

1. If `r_samp_freq != cast_handle->sampling_freq`, store the new rate and call
   `GraphicEqReCalcAllBandCoeffs()` — i.e. **coefficient redesign happens on the audio thread**,
   inside the callback, the first buffer after a rate change (`GraphicEqProcess.cpp:49-54`).
   That redesign calls `pow`, `tan`, `asin`, `atan2`, `sqrt` once per band.
2. `i_num_channels <= 2` → `sosProcessBuffer`; `== 6 || == 8` → `sosProcessSurroundBuffer`;
   anything else → error (`GraphicEqProcess.cpp:57-68`).

`GraphicEqProcess_MasterGainOnly` skips the filters entirely and just multiplies by `master_gain`
(`SosProcess.cpp:501-517`).

Input and output pointers are the **same buffer** at the call site (`rp_samples, rp_samples`,
`dfxpProcessReal.cpp:153`) — the SOS loop is in-place-safe because it reads `rp_in_buf[k]` before
writing `rp_out_buf[k]`.

---

## 3. Band-count and band-frequency layout

`DFXP_GRAPHIC_EQ_NUM_BANDS` is a **mutable global `int`**, initialised to `31`
(`DfxDspEq.cpp:32`), and rewritten from inside `GraphicEqSetNumBands` (`GraphicEqSet.cpp:154`).
The GUI offers `{5, 10, 15, 20, 31}` (`fxsound/Source/GUI/FxAudioControls.h:106`) with a default of
`10` (`fxsound/Source/GUI/FxController.h:46`).

Defaults from `GraphicEq.h`:

| Define | Value | Cite |
|---|---|---|
| `GRAPHIC_EQ_DEFAULT_FIRST_BAND_FREQ` | `20` | `GraphicEq.h:36` |
| `GRAPHIC_EQ_DEFAULT_LAST_BAND_FREQ` | `20000.0` | `GraphicEq.h:37` |
| `GRAPHIC_EQ_DEFAULT_SAMPLING_FREQ` | `44100.0` | `GraphicEq.h:39` |
| `GRAPHIC_EQ_MIN_BAND_FREQ` | `10` | `GraphicEq.h:42` |
| `GRAPHIC_EQ_MAX_BAND_FREQ` | `21000.0` | `GraphicEq.h:43` |
| `GRAPHIC_EQ_DEFAULT_MAX_BOOST_OR_CUT` | `20.0` dB | `GraphicEq.h:46` |
| `GRAPHIC_EQ_MAX_NUM_BANDS` = `SOS_MAX_NUM_SOS_SECTIONS` | `32` | `u_GraphicEq.h:35`, `sos.h:27` |

### 3.1 Hard-coded centre-frequency tables

`GraphicEqReSetAllBandFreqs` (`GraphicEqSet.cpp:362-528`) branches on `num_bands` and **overwrites**
`min_band_freq`/`max_band_freq` from the table it picks.

**5 bands** — `GraphicEqSet.cpp:430-440`, min `62.5`, max `16000`:

```
62.5, 250, 1000, 4000, 16000                                      (exact octave-times-2 ladder, r = 4)
```

**10 bands** — `GraphicEqSet.cpp:441-455`, min `62.5`, max `16000`. Explicitly *not* ISO-266; the
comment at `:443-446` says this grid is what the factory/community presets were authored against:

```
62.5, 115.734, 214.311, 396.85, 734.867, 1360.79, 2519.84, 4666.12, 8640.48, 16000.0
```

**15 bands (ISO)** — `GraphicEqSet.cpp:456-467`, min `25`, max `16000`:

```
25, 40, 63, 100, 160, 250, 400, 630, 1000, 1600, 2500, 4000, 6300, 10000, 16000
```

**20 bands (ISO subset)** — `GraphicEqSet.cpp:468-479`, min `20`, max `16000`:

```
 20,  31.5,  40,  63,  80, 125,  160,  250,  315,   500,
630, 1000, 1250, 2000, 2500, 4000, 5000, 8000, 10000, 16000
```

**31 bands (full ISO third-octave)** — `GraphicEqSet.cpp:480-492`, min `20`, max `20000`:

```
  20,   25,  31.5,   40,   50,   63,   80,  100,  125,  160,
 200,  250,  315,   400,  500,  630,  800, 1000, 1250, 1600,
2000, 2500, 3150,  4000, 5000, 6300, 8000,10000,12500,16000, 20000
```

**Any other band count** falls through to a pure geometric ladder (`GraphicEqSet.cpp:493-509`),
computed in `double`:

```
f[i] = min_band_freq * (max_band_freq / min_band_freq) ^ ( i / (N - 1) )     i = 0 .. N-1
```

`num_bands == 1` is a special case handled before the table: `Q` is forced to `1.0` and the single
band sits at `min_band_freq` (`GraphicEqSet.cpp:386-393`).

### 3.2 Per-band frequency override and clamping

`GraphicEqSetBandFreq(handle, band_num /*1-based*/, freq)` (`GraphicEqSet.cpp:537-578`):

* rejects `band_num <= 0 || band_num > num_bands`,
* clamps `freq` to `[10, 21000]` (`GraphicEqSet.cpp:555-559`),
* stores into `sos_center_freq[band-1]`,
* **zeroes** `sos_center_freq_response[band-1]` and then re-applies the saved boost — a deliberate
  trick to defeat the `if (r_boost_cut != rp_boost_array[...])` early-out and force a coefficient
  refresh (`GraphicEqSet.cpp:570-575`).

Presets can carry their own band frequencies; `DfxDspEq.cpp:236-240` applies them **only when the
preset's band count matches the live band count**. On a mismatch the live ISO/geometric table is
kept and only the gains are interpolated (`DfxDspEq.cpp:168-175`, `189-227`).

### 3.3 Displayed band edges (for the UI slider ranges)

`GraphicEqGetBandFrequencyRange` (`GraphicEqGet.cpp:105-168`) — half-step geometric edges:

```
ratio  = max_band_freq / min_band_freq
band 1 : min edge  = min_band_freq                                (pinned, :137)
else   : min edge  = round( min_band_freq * ratio^( ((b-1)*2 - 1) / (N*2 - 2) ) )
                     then +1 if < 1000 Hz, else +10                (:145-149)
band N : max edge  = max_band_freq                                (pinned, :155)
else   : max edge  = round( min_band_freq * ratio^( (b*2 - 1) / (N*2 - 2) ) )   (:159-163)
```

The `+1` / `+10` nudge on the *min* edge only is asymmetric and produces a 1 Hz / 10 Hz gap between
adjacent bands' ranges. Reproduce it if you want the same slider ranges.

---

## 4. Q derivation — band spacing × Q multiplier

`GraphicEqSet.cpp:512-525`, computed in `double`, stored to `f32`:

```
d_min_freq = min_band_freq
d_ratio    = max_band_freq / min_band_freq
r          = d_ratio ^ ( 1 / (N - 1) )        // geometric step between adjacent bands
Q          = sqrt(r) / (r - 1)                // ideal constant-Q graphic EQ
Q         *= Q_multiplier
if (Q < 1.0) Q = 1.0                          // hard minimum
```

`r` is the ratio between adjacent band centres, so `sqrt(r)` is the geometric half-step and
`r - 1` is the fractional bandwidth of one band spacing. `Q = f0/BW` with `BW = f0(r-1)/sqrt(r)`
gives exactly this — the bands' −3 dB (or −half-boost) edges meet at the geometric midpoints.

`Q_multiplier` defaults to `1` (`GraphicEqInit.cpp:49`), is exposed by
`GraphicEqSetFilterQ`/`GraphicEqGetFilterQ` (`GraphicEqSet.cpp:106-116`, `GraphicEqGet.cpp:213-225`),
and the GUI slider is `range 1 .. 3 step 0.5` (`fxsound/Source/GUI/FxAudioControls.cpp:348`) with
default `1.0f` (`fxsound/Source/GUI/FxController.h:50`). The controller additionally rejects
anything outside `[1, 3]` when loading (`fxsound/Source/GUI/FxController.cpp:328, 495`).

**Setting `Q_multiplier` re-runs `GraphicEq_InitSections`**, which resets the frequency table to the
`GRAPHIC_EQ_DEFAULT_*` values (20 Hz / 20 kHz / 44100 Hz) before recomputing
(`GraphicEqSet.cpp:115` → `GraphicEqInitSections.cpp:43-52`). This silently discards any
preset-supplied per-band frequencies and resets `sampling_freq` to 44100 until the next buffer
corrects it. Flag this; it is almost certainly a bug you do not want to port verbatim.

### 4.1 Reference Q table **(computed)**

| N | min (Hz) | max (Hz) | ratio | `r = ratio^(1/(N-1))` | raw `Q` | `Q` after ×1 and the ≥1.0 clamp |
|---:|---:|---:|---:|---:|---:|---:|
| 5  | 62.5 | 16000 | 256  | 4.00000000 | 0.66666667 | **1.00000000** (clamped) |
| 10 | 62.5 | 16000 | 256  | 1.85174942 | 1.59764123 | 1.59764123 |
| 15 | 25   | 16000 | 640  | 1.58650493 | 2.14757848 | 2.14757848 |
| 20 | 20   | 16000 | 800  | 1.42165498 | 2.82774260 | 2.82774260 |
| 31 | 20   | 20000 | 1000 | 1.25892541 | 4.33336553 | 4.33336553 |

The 15/20/31 values match the dead comments in the source exactly
(`2.14757848`, `2.82774258`, `4.33336544` at `GraphicEqSet.cpp:410, 415, 421`), which confirms the
formula. The 5-band case is the only one where the `Q < 1.0 → 1.0` clamp fires.

With `Q_multiplier = 3` the 31-band Q becomes `13.00009659` before the design-time limiters
described in §6.1 kick in.

---

## 5. Boost / cut range and the clamp chain

A band gain passes through **three** independent clamps:

| Stage | Range | Cite |
|---|---|---|
| GUI slider | `-12 .. +12` dB, step `1.0` | `fxsound/Source/GUI/FxEqualizer.cpp:48, 98`; `MAX_GAIN = 12.0f` at `FxEqualizer.h:105` and `FxController.h:53` |
| `dfxpEqSetBandBoostCut` | `[-12.0, +12.0]` dB | `dfxpEq.cpp:245-248`, constants at `dfxpDefs.h:264-265` |
| `GraphicEqSetBandBoostCut` | `[-20.0, +20.0]` dB | `GraphicEqSet.cpp:298-302`, constant `GraphicEq.h:46` |

So the effective user range is **±12 dB**, but the DSP layer will accept ±20 dB if driven directly.

Two more gates in `GraphicEqSetBandBoostCut` (`GraphicEqSet.cpp:288-295`) **bypass the filter
entirely** and install a unity-gain section:

```
if (r_boost_cut == 0.0f)  ||  (band_freq * 2.0f >= sampling_freq)   ->  sosSetSectionUnityGain()
```

The second condition is a Nyquist guard: a 20 kHz band is disabled at 44.1 kHz
(`20000*2 = 40000 >= 44100` is false — so it is **not** disabled at 44.1 k; it is disabled only at
≤ 40 kHz rates). Watch the exact comparison, it is `>=` on `2*f`, not `>` on `f`.

Recalculation is guarded by an **exact float equality** test
(`if (r_boost_cut != rp_boost_array[i_section_num])`, `GraphicEqSet.cpp:305`) — hence the
"zero the stored boost then re-set it" idiom used at `GraphicEqSet.cpp:343-347` and `:570-574`.

`GraphicEqReCalcAllBandCoeffs` (`GraphicEqSet.cpp:321-352`) is the "sample rate changed" path: for
each section it copies the stored boost, zeroes the stored value, and calls
`GraphicEqSetBandBoostCut` again.

### 5.1 Registry round-trip quirk (important for preset fidelity)

`dfxpEqSetBandBoostCut` writes the value as `L"%.2f"` (`dfxpEq.cpp:274`), and
`eqUpdateFromRegistry` compares the *string* forms of the in-memory and on-disk values rather than
the floats (`DfxDspEq.cpp:93-95`, `wcscmp`). So band gains are effectively quantised to two decimal
places on every persistence round trip, and the comparison is locale-sensitive (`%.2f` uses the C
locale's decimal point under MSVC's `swprintf`; a comma-decimal locale would break equality
detection).

---

## 6. Filter topology

### 6.1 Structure

**One biquad (second-order section) per band, cascaded in series, Direct Form II Transposed**, with
the numerator/denominator `z^-1` coefficients tied together (`a1 == b1`) — see §6.3.

Transfer function, as documented in the source headers (`FiltCalcBiqd.cpp:90-96`,
`u_sos.h:29-34`):

```
                b0 + b1 z^-1 + b2 z^-2
       T(z) = --------------------------          a0 normalised to 1
                 1 + a1 z^-1 + a2 z^-2
```

**Denominator coefficients are NOT negated** here. (Contrast `Fil12But.cpp` / `FiltRun.cpp`, which
*do* use pre-negated denominators — see §11. Do not mix the two conventions.)

Cascade layout for `N` bands:

```
        ┌────────┐   ┌────────┐          ┌────────┐
 in ───►│ sec 0  ├──►│ sec 1  ├── ... ──►│ sec N-1├──► × master_gain ──► × balance_L/R ──► × norm_gain ──► volume levelling ──► out
        │ f=20Hz │   │ f=25Hz │          │f=20kHz │
        └────────┘   └────────┘          └────────┘
          state1        state1              state1      (channel 1)
          state2        state2              state2
          state3        state3              state3      (channel 2)
          state4        state4              state4
```

Sections whose `section_on_flag` is `IS_FALSE` are **skipped entirely** (their state is left stale;
it is not advanced). See §8 for why this matters when a band is re-enabled.

### 6.2 Design-time Q limiters (inside `filtCalcParametric`)

Before any coefficient maths, `filtCalcParametric` mutates `f->Q` downward under two rules
(`FiltCalcBiqd.cpp:146-176`). These constants are local `#define`s inside the function body:

| Define | Value | Cite |
|---|---|---|
| `FILT_Q_UPPER_LIMIT_FREQ` | `60.0` Hz | `FiltCalcBiqd.cpp:146` |
| `FILT_Q_LOWER_LIMIT_FREQ` | `20.0` Hz | `FiltCalcBiqd.cpp:147` |
| `FILT_Q_UPPER_LIMIT` | `20.0` | `FiltCalcBiqd.cpp:148` |
| `FILT_Q_LOWER_LIMIT` | `1.0` | `FiltCalcBiqd.cpp:149` |
| `FILT_Q_LIMIT_SCALE` | `(20.0-1.0)/(60.0-20.0)` = `0.475` | `FiltCalcBiqd.cpp:150` |
| `FILT_BOOST_WARP_LEVEL` | `6.0` dB | `FiltCalcBiqd.cpp:152` |
| `FILT_BOOST_MAX_Q` | `20.0` | `FiltCalcBiqd.cpp:153` |
| `FILT_BOOST_MIN_Q` | `0.2` | `FiltCalcBiqd.cpp:154` |
| `FILT_BOOST_SCALE` | `(20.0-0.2)/6.0` = `3.3` | `FiltCalcBiqd.cpp:155` |

**Rule A — low-frequency Q limit** (`FiltCalcBiqd.cpp:160-166`). Guards the `f32` cancellation
described in §1:

```
if (f0 < 60.0) {
    maxQ = (f0 - 20.0) * 0.475 + 1.0        // 20 Hz -> 1.0 ; 60 Hz -> 20.0 ; linear in Hz
    if (Q > maxQ) Q = maxQ
}
```

Note this can go **negative** for `f0 < 20 Hz` (a 10 Hz band gives `maxQ = -3.75`). The 5/10-band
tables start at 62.5 Hz so they never hit it, but the 20/31-band tables start exactly at 20 Hz
(`maxQ = 1.0`) and a preset-supplied or user-dragged frequency below 20 Hz is legal down to the
10 Hz clamp of `GraphicEqSetBandFreq` — at which point `Q` goes negative,
`bandwidth = center_freq / Q` goes negative, and `filtBW2ANGLE` is fed a negative bandwidth. See
§18.

**Rule B — low-boost Q warp** (`FiltCalcBiqd.cpp:168-176`). The comment at
`FiltCalcBiqd.cpp:115-121` explains the motive: at small boosts the designed numerator and
denominator nearly cancel, and at high Q the `f32` cancellation "boinks":

```
abs_boost = |boost|
if (abs_boost < 6.0) {
    maxQ = abs_boost * 3.3 + 0.2            // 0 dB -> 0.2 ; 6 dB -> 20.0
    if (Q > maxQ) Q = maxQ
}
```

So a ±1 dB band is always designed with `Q ≤ 3.5`, a ±2 dB band with `Q ≤ 6.8`, etc., regardless of
the graphic-EQ Q. This gives the EQ its characteristic "wide at small gains, narrow at large gains"
behaviour and is *audible* — do not drop it.

These mutations are applied to a **copy**: `filtSosParametric` takes `Q` by value into a local
`struct filt2ndOrderBoostCutShelfFilterType` (`FiltbiqdSos.cpp:53-58`), so `GraphicEqHdlType::Q`
is never modified.

### 6.3 Coefficient derivation (parametric peaking) — `filtCalcParametric`

`FiltCalcBiqd.cpp:109-222`. Plain maths, no LaTeX:

**Step 0 — zero-boost bypass** (`:132-137`)

```
if (boost == 0.0) {
    section_on_flag = IS_FALSE
    b0 = 1.0 ;  b1 = b2 = a1 = a2 = 0.0
    return
}
```

**Step 1 — normalise frequency, derive bandwidth** (`:178-179`)

```
w0 = f0 / fs                    // normalised, cycles/sample, in (0, 0.5)
bw = w0 / Q                     // normalised bandwidth, cycles/sample
```

**Step 2 — bilinear warp factor** (`:181-182`)

```
a   = tan( PI * (w0 - 0.25) )       // in (-1, +1); a = 0 exactly at fs/4
asq = a * a
```

This is the allpass coefficient of the frequency-shifting substitution
`z^-1  ->  (a + z^-1) / (1 + a·z^-1)`. The prototype filter is designed centred at `fs/4`
(normalised 0.25, where the bilinear transform is exact and the `f32` maths is well conditioned)
and then conformally mapped to `f0`. That is the "pre-warping" in this design: **there is no
separate `tan(w0/2)` prewarp**, the warp is folded into `a`.

* `w0 → 0`  ⇒ `a → tan(-π/4) = -1`
* `w0 = 0.25` ⇒ `a = 0`
* `w0 → 0.5`  ⇒ `a → tan(+π/4) = +1`

**Step 3 — dB to linear, pick the bandedge reference** (`:183-188`)

```
A = 10 ^ (boost / 20)                      // linear amplitude factor

if (-6.0 < boost < 6.0)   F = sqrt(A)      // bandedge at HALF the boost in dB
else if (A > 1.0)         F = A / sqrt(2)  // bandedge at boost - 3 dB
else                      F = A * sqrt(2)  // bandedge at boost + 3 dB (cut case)
```

Source comment (`:189-191`): "If |boost/cut| < 6dB, then doesn't make sense to use 3dB pt. use of
root makes bandedge at half the boost/cut amount." Note the boundary is **exclusive on both sides**
— exactly `±6.0` dB takes the `sqrt(2)` branch.

**Step 4 — map bandwidth to the prototype's bandedge** (`:192`)

```
xfmbw = filtBW2ANGLE(a, bw)      // see §7; normalised frequency in (0, 0.25]
C     = 1.0 / tan( 2*PI * xfmbw )    // cotangent
```

**Step 5 — alpha (the pole/zero radius parameters)** (`:195-201`)

```
F2  = F*F
tmp = A*A - F2
if (|tmp| <= SPN)   alphad = C
else                alphad = sqrt( C*C * (F2 - 1.0) / tmp )
alphan = A * alphad
```

Closed forms per branch (useful as a sanity check and as a cheaper implementation):

* `|boost| < 6`: `F2 = A`, `tmp = A(A-1)`, so `alphad = C / sqrt(A)` and `alphan = C * sqrt(A)`.
* `boost >= 6`: `F2 = A²/2`, `tmp = A²/2`, so `alphad = C * sqrt(1 - 2/A²)`.
* `boost <= -6`: `F2 = 2A²`, `tmp = -A²`, so `alphad = C * sqrt(1/A² - 2)` (well-defined because
  `A ≤ 10^-0.3 = 0.5012 < 1/sqrt(2)`).

The `|tmp| <= SPN` guard only fires when `boost` is denormal-small, which cannot happen because
`boost == 0.0` was already handled in Step 0.

**Step 6 — build the mapped biquad** (`:203-218`)

```
a2plus1  = 1.0 + asq
ma2plus1 = 1.0 - asq

b0_raw = a2plus1 + alphan * ma2plus1
b1_raw = 4.0 * a
b2_raw = a2plus1 - alphan * ma2plus1

a0_raw = a2plus1 + alphad * ma2plus1
a2_raw = a2plus1 - alphad * ma2plus1

recip = 1.0 / a0_raw
b0 = b0_raw * recip
b1 = b1_raw * recip
b2 = b2_raw * recip
a1 = b1                       // <-- THE KEY SYMMETRY, FiltCalcBiqd.cpp:217
a2 = a2_raw * recip
```

**`a1` is literally assigned from the already-normalised `b1`.** The denominator's `z^-1` term is
`4a` too (the allpass substitution produces the same cross term for numerator and denominator
because only the `alphan`/`alphad` factors differ, and those multiply `1 - a²`, not `a`). This is
the symmetry the SOS inner loop exploits — see §8. If you change anything about the design you
must re-derive that loop.

Sanity check against a textbook peaking EQ: at `f0 = 1 kHz`, `fs = 48 kHz`,
`a = tan(π(1/48 - 0.25)) = -0.877`, giving `b1 = a1 ≈ -1.958`. A resonator at 1 kHz should have
`a1 = -2r·cos(ω0) = -2 · 0.987 · cos(0.1309) ≈ -1.957`. ✓

### 6.4 Shelving design — `filtCalcShelf`

`FiltCalcBiqd.cpp:248-340`. **Not used by the graphic EQ** (only `filtSosParametric` is called from
`GraphicEqSetBandBoostCut`, `GraphicEqSet.cpp:307`). `filtSosShelf`/`filtCalcShelf` are reachable
only through `qntIToBoostCutInit` with `i_filter_type != FILT_BOOST_CUT`
(`QntitoBoostCut.cpp:96-100`), and the single caller passes `FILT_BOOST_CUT`
(`dfxpQnt.cpp:537`). Document it anyway — a Linux port will likely want shelves.

```
FILT_LO_SHELF  = 0        filt.h:63
FILT_HI_SHELF  = 1        filt.h:64
FILT_BOOST_CUT = 2        filt.h:65
```

```
w0  = f0 / fs
a   = tan( PI * (w0 - 0.25) ) ;  asq = a*a          // same warp factor
A   = 10 ^ (boost/20)
F   = same 3-way branch as §6.3 step 3

F2  = F*F ;  tmp = A*A - F2
if (|tmp| <= SPN)  gammad = 1.0
else               gammad = ( (F2 - 1.0) / tmp ) ^ 0.25      // FOURTH root
gamman = sqrt(A) * gammad

// numerator prototype (2nd-order Butterworth shelf at fs/4)
g2   = gamman*gamman ;  g2p1 = 1.0 + g2 ;  sig = 2*ROOT2O2 * gamman
ta0  = g2p1 + sig
ta1  = -2.0 * (1.0 - g2)   ;  if (high_or_low) ta1 = -ta1
ta2  = g2p1 - sig

// denominator prototype (same shape, gammad)
g2   = gammad*gammad ;  g2p1 = 1.0 + g2 ;  sig = 2*ROOT2O2 * gammad
tb0  = g2p1 + sig
tb1  = -2.0 * (1.0 - g2)   ;  if (high_or_low) tb1 = -tb1
tb2  = g2p1 - sig

// bilinear-transform both to f0
b0_raw = ta0 + a*ta1 + asq*ta2
b1_raw = 2*a*(ta0 + ta2) + (1 + asq)*ta1
b2_raw = asq*ta0 + a*ta1 + ta2
a0_raw = tb0 + a*tb1 + asq*tb2
a1_raw = 2*a*(tb0 + tb2) + (1 + asq)*tb1
a2_raw = asq*tb0 + a*tb1 + tb2

divide all five by a0_raw                       // a1 is NOT tied to b1 here
```

`2*ROOT2O2` = `1.4142135623930950488` = the Butterworth `sqrt(2)` damping.

**Critical:** the SOS inner loop (§8) assumes `a1 == b1`. `SosProcess.cpp:525-526` says so
explicitly: *"this appears to have a serious problem with shelf functions, the processing method
uses a form specific to the coeff symmetry that occurs with parametric filters."*
If you add shelves in Rust you must use a general DF2T that actually uses `a1`.

---

## 7. `filtBW2ANGLE` — bandwidth → prototype bandedge

`FiltCalcBiqd.cpp:55-82`. Given the warp factor `a` and the desired normalised bandwidth at `f0`,
returns the bandedge `e` (normalised frequency, 0..0.25) that the prototype — centred at 0.25 —
must have so that, after the allpass substitution, the realised bandwidth equals `bandwidth`.

```
T   = tan( 2*PI * bandwidth )
a2  = a*a ;  a4 = a2*a2
d   = 2*a2*T
sn  = (1 + a4) * T
cs  = (1 - a4)
mag = sqrt(sn*sn + cs*cs)
d  /= mag
delta = atan2(sn, cs)
asnd  = asin(d)

theta = 0.5 * (PI - asnd - delta)       // bandedge for the prototype
tmp   = 0.5 * (asnd - delta)            // principal branch
if (tmp > 0.0 && tmp < theta)  theta = tmp

if (bandwidth >= 0.5)  theta = 0.005    // HACK, see below

return theta / (2*PI)                   // normalised frequency
```

The source comment at `FiltCalcBiqd.cpp:48-53` documents the failure mode: *"At low Q settings
(example, Q=.36, samp_f = 48k, boost=15, center_freq=8869.84) this function acts wrong… when
bandwidth >= 0.5, the function acts incorrectly. The tmp val goes neg, and theta stops getting set
to tmp… ADDED A HACK."* The `bandwidth >= 0.5` override forces an extremely narrow prototype
bandedge (`theta = 0.005` rad, i.e. `e = 0.000796`), which makes `C = 1/tan(2π·e) ≈ 199.9` — a very
high-Q section. Port it verbatim; it is reachable whenever `w0/Q >= 0.5`, e.g. a 20 kHz band at
44.1 kHz with `Q < 0.907`. In practice the ≥1.0 Q floor plus the Nyquist gate make it rare, but
`asin(d)` with `|d| > 1` returning NaN is the alternative, so keep the guard.

`asin(d)` can also produce NaN if rounding pushes `d` marginally above 1. In `f32` with
`d = 2a²T/sqrt((1+a⁴)²T² + (1-a⁴)²)` this is mathematically bounded by 1, but clamp it in Rust.

---

## 8. The per-sample inner loop — exact state update

`u_sos.h:47-62` defines the section:

```c
struct sosSectionType {
   realtype b0, b1, b2, a1, a2;          // a1 stored but UNUSED by the process loop
   realtype a1_old, a2_old;              // previous coeffs, stored by sosSetSection, never read back
   realtype state1, state2;              // channel 1 (left / mono)
   realtype state3, state4;              // channel 2 (right)
   realtype state_1[8], state_2[8];      // surround, one pair per channel
};
```

Constants (`u_sos.h:41-44`):

| Define | Value | Purpose |
|---|---|---|
| `SOS_FLOAT_BIAS` | `1.0e-30` | denormal defeat, added to every section output every sample. Comment: *"Was 1.0e-5 Thru 7/11/15, then changed to 1.0e-30 values that was being used in Hyperbass."* |
| `SOS_DCBLOCK_ALPHA` | `0.999` | DC blocker pole — **compiled out**, see below |
| `SOS_VOLUME_LEVELING_HISTORY_SIZE` | `6` | leveller power ring buffer |
| `SOS_VOLUME_LEVELING_PEAK_WINDOW_SIZE` | `30` | leveller 1-second peak buckets |

### 8.1 Mono — `SosProcess.cpp:552-583`

```c
in1  = rp_in_buf[k];
out1 = in1;                                   // pass-through if every section is off
for (i = 0; i < num_active_sections; i++) {
    if (!section_on_flag[i]) continue;        // state NOT advanced for off sections
    s = &sections[i];
    out1      = s->state1 + s->b0 * in1 + (realtype)SOS_FLOAT_BIAS;   // :576
    s->state1 = (in1 - out1) * s->b1 + s->state2;                     // :577
    s->state2 = s->b2 * in1 - s->a2 * out1;                           // :578
    in1 = out1;
}
rp_out_buf[k] = out1 * master_gain;                                   // :583
```

Unpacking line `:577` with the `a1 == b1` identity from §6.3:

```
state1 = b1*in - b1*out + state2
       = b1*x[n] - a1*y[n] + state2            (since a1 == b1)
```

which is exactly canonical **Direct Form II Transposed**:

```
y[n]  = b0·x[n] + s1[n-1]
s1[n] = b1·x[n] - a1·y[n] + s2[n-1]
s2[n] = b2·x[n] - a2·y[n]
```

The rewrite `(in - out) * b1` saves one multiply per sample per section — and hard-codes the
symmetry.

ASCII of the topology actually implemented:

```
        x[n] ──┬──────────── ×b0 ──────────►(+)──┬──► y[n]   (+1e-30 bias injected here)
               │                              ▲  │
               │                              │  │
               │                          [state1]│
               │                              ▲  │
               ├── ×b1 ──►(+)◄── ×(-b1) ──────┼──┤        ← (in - out)*b1 == b1·x - a1·y
               │           ▲                  │  │
               │       [state2]               │  │
               │           ▲                  │  │
               └── ×b2 ──►(+)◄── ×(-a2) ──────┴──┘
```

### 8.2 Stereo — `SosProcess.cpp:585-636`

Identical, twice, with `state3/state4` for the right channel (`:617-627`). Then:

```c
rp_out_buf[k]     = out1 * master_gain * balance_left;    // :630
rp_out_buf[k + 1] = out2 * master_gain * balance_right;   // :631
if (target_rms != 1.0f)
    sum_squares += out_L*out_L + out_R*out_R;             // :633-636
```

Samples are **interleaved**; `k += i_num_channels` per frame (`:639`).

### 8.3 Surround (6 / 8 ch) — `SosProcess.cpp:846-911`

Channel order documented at `:862-863`:
`FL, FR, FC, LFE, BL, BR [, SL, SR]`.

Band routing (`:873-883`):

```
channel 3 (LFE) : sections [0, 2)                       // only the bottom two bands
all others      : sections [2, num_active_sections)     // everything except the bottom two
```

Same three-line update using `state_1[k] / state_2[k]` (`:897-899`). Output is
`out * master_gain` only — **no balance, no normalisation** on the surround path (`:904`).

### 8.4 DC blocker — present but dead

`SosProcess.cpp:554-563` and `:587-602` are wrapped in `#ifdef SOS_DO_DC_BLOCKING`. A tree-wide
grep finds the symbol only at those four `#ifdef`/`#endif` lines and nowhere in
`dsp/DfxDsp.vcxproj` (whose `PreprocessorDefinitions` are
`WIN32;NDEBUG;_LIB;PT_NON_MFC;DSPSOFT_TARGET;PT_DSP_BUILD=PT_DSP_DFX`, `DfxDsp.vcxproj:165`).
**The DC blocker is compiled out.** The code it would run:

```
in  = x[n] - in_old + 0.999 * outDC_old
in_old    = x[n]
outDC_old = in
```

i.e. a one-pole/one-zero highpass with pole at 0.999 (≈3.5 Hz at 44.1 kHz). Do not enable it in the
Rust port unless you intend to change the sound; without it, the `1e-30` bias accumulates as a tiny
DC offset (see §18).

### 8.5 Post-filter gain stages

| Stage | Formula | Cite |
|---|---|---|
| master gain | `linear = 10^(gain_db/20)`, applied per sample | `GraphicEqSet.cpp:101-103`; GUI range `-20..+20` step `2` at `FxAudioControls.cpp:312` |
| balance | `bal_db > 0 → L = 10^(-bal_db/20), R = 1` ; `bal_db < 0 → L = 1, R = 10^(bal_db/20)` | `GraphicEqSet.cpp:47-59`; GUI clamps `[-20, +20]` at `FxController.cpp:316` |
| normalisation target | `target_rms = 10^(gain_db/20)` stored as `sosHdlType::target_rms` | `GraphicEqSet.cpp:71-73`, `SosSet.cpp:276` |
| volume levelling | `target_rms = (clamp(v, 0, 4) / 4) * 0.5` | `GraphicEqSet.cpp:83-89`, constants `GraphicEqSet.cpp:34-35`; GUI range `0..4` step `0.5` at `FxAudioControls.cpp:330` |

### 8.6 Buffer-level RMS normalisation

`SosProcess.cpp:678-723`, **stereo only**, and only when `target_rms != 1.0f`:

```
current_rms = sqrt(sum_squares / (num_sample_sets * 2))
if (current_rms < 1e-6)  current_rms = 1e-6                 // :683
target_gain = target_rms / current_rms
target_gain = clamp(target_gain, 0.01, 1.0)                 // min -40 dB, max 0 dB   :688-691
gain_diff   = |target_gain - normalization_gain|

if (target_gain > normalization_gain)  smoothing = 0.0005 + gain_diff*0.001   // slow attack :701
else                                   smoothing = 1.0                        // instant release :706
smoothing = min(smoothing, 0.5)                                               // :710
normalization_gain += (target_gain - normalization_gain) * smoothing          // :713
apply normalization_gain to every sample of the buffer                        // :716-722
```

Note the asymmetry: `smoothing = 1` on release means the gain snaps down instantly, then creeps
back up at ~0.0005/buffer. The `min(smoothing, 0.5)` on line `:710` is applied *after* the `= 1`
assignment, so the release factor is really `0.5`, not `1.0`. Reproduce carefully; the `= 1` looks
like a debug leftover.

Finally `applyVolumeLeveling(...)` runs (`:725` stereo, `:908` surround, with the LFE channel index
`3` excluded on the surround path). That detector is a separate subsystem (a sidechain HPF at
120 Hz, three one-pole tone probes at 180 / 1200 / 4500 Hz, a 6-entry power history and a 30-second
peak window — `SosProcess.cpp:37-76`, `u_sos.h:80-104`) and is out of scope for this document.

---

## 9. Section lifecycle and flags

`sosNew(handle, slout, n)` (`Sos.cpp:40-106`):

```
num_allocated_sections = n
num_active_sections    = n
master_gain            = 1.0
balance_left/right     = 1.0
target_rms             = 1.0          // == "normalisation off" sentinel
normalization_gain     = 1.0
volume_leveling_target_rms = 0.0 ; volume_leveling_gain = 1.0
then sosSetAllSectionsUnityGain(handle, IS_TRUE) and sosZeroStateAllSections(handle)
```

`sosSetSectionUnityGain` (`SosSet.cpp:88-113`) installs
`b0=1, b1=b2=a1=a2=0, boost=0, center_freq=20.0, section_on_flag=IS_FALSE` — and with
`i_init_freq = IS_TRUE` it also **stomps the section's centre frequency to 20.0**. `GraphicEq`
always calls it with `0` (`GraphicEqSet.cpp:291`), so the frequency survives there; `sosNew` calls
it with `IS_TRUE`, so all sections start at 20 Hz.

`sosSetSection` (`SosSet.cpp:40-81`) stores `a1_old`/`a2_old` before overwriting — those fields are
never read anywhere in the tree (they were presumably for a coefficient-crossfade that was never
finished). Skip them in Rust.

`sosZeroStateAllSections` (`SosSet.cpp:142-216`) zeroes `state1..state4`, `state_1[0..8]`,
`state_2[0..8]`, the DC-blocker memories, and the entire volume-levelling detector state.

---

## 10. Frequency-response evaluation (for drawing the EQ curve)

`Filtpoly.cpp`. Everything here is `realtype` (`f32`) except where noted.

```
filtPolyCalc2ndOrderResponse(c0, c1, c2, f)                          Filtpoly.cpp:30-40
    omega     = MTH_TWO_PI  * f          // f is NORMALISED frequency (cycles/sample)
    two_omega = MTH_FOUR_PI * f
    real = c0*cos(two_omega) + c1*cos(omega) + c2
    imag = c0*sin(two_omega) + c1*sin(omega)
    return sqrt(real² + imag²)
```

Note the convention: this evaluates `c0·z² + c1·z + c2` (positive powers). Magnitude is unaffected
by the `z²` prefactor, so it agrees with `c0 + c1 z^-1 + c2 z^-2` — the source comment at
`Filtpoly.cpp:143-152` confirms the author checked both.

```
filtPolyCalcBiquadResponse(b0,b1,b2, a0,a1,a2, f)      = num/den          :66-75
filtPolyCalcBiquadResponseFiltStruct(f_struct, f)      = num/den with a0 forced to 1.0   :83-92
filtPolyCalc2ndOrderPowerResponse(...)                 = real² + imag²  (no sqrt)        :48-58
filtPolyCalcBiquadPowerResponse / ...FiltStruct        power ratio                        :100-126
filtPolyCalc2ndOrderPowRespCosSupplied(c0,c1,c2, cosw, sinw, cos2w, sin2w)                :137-157
filtPolyCalcBiquadPowRespCosSupplied(...)              power ratio, trig precomputed      :165-178
```

The `CosSupplied` variants exist so a plot of N frequency points can share one trig table across
all 31 bands. In Rust: precompute `(cos ω, sin ω, cos 2ω, sin 2ω)` per pixel column once, then loop
bands.

`filtCalcFirResponse(coeffs, n, f)` (`FiltCalcFilterResponse.cpp:30-49`) — the only `double`
routine in the whole `Filt` directory:

```
omega = MTH_TWO_PI * f
real  = Σ c[i]·cos(i·omega)
imag  = -Σ c[i]·sin(i·omega)
return sqrt(real² + imag²)
```

---

## 11. Butterworth designers and run functions (shared filter maths)

These are **not** used by the graphic EQ but are the shared 1st/2nd-order building blocks the rest
of the DSP (stereo widener, hyperbass, spectrum) leans on. They use a **negated-denominator**
convention, the opposite of §6 — the header comments say so repeatedly
(`FiltRun.cpp:28, 44, 62, 79`; `Fil12But.cpp:65, 87, 102, 138`).

`r_omega` is a **normalised radian frequency**, documented as `0 -> 2PI`
(`Fil12But.cpp:34, 52, 74, 95, 120`), and the callers build it as
`omega = TWO_PI * freq_hz * sampling_period` (`Qnt2But.cpp:175`).

### 11.1 Designers — `Fil12But.cpp`

| Function | Structure | Coefficients | Cite |
|---|---|---|---|
| `filtDesignSimple1rstLowPass(ω, &a0)` | `(1-a0)/(1 - a0·z^-1)` | `cos_om = cos(ω)`; `a0 = 2 - cos_om - sqrt(cos_om² - 4·cos_om + 3)` | `Fil12But.cpp:37-46` |
| `filtDesign1rstLowPass(ω, &g, &a0)` | `g(z+1)/(z-a0)` | `t = 1/(2+ω)`; `g = ω·t`; `a0 = (2-ω)·t` | `Fil12But.cpp:60-67` |
| `filtDesign1rstHighPass(ω, &g, &a0)` | `g(z-1)/(z-a0)` | `t = 1/(2+ω)`; `g = 2·t`; `a0 = (2-ω)·t` | `Fil12But.cpp:82-89` |
| `filtDesign2ndButLowPass(ω, &g, &a1, &a0)` | `g(z²+2z+1)/(z² - a1·z - a0)` | `ω² `; `r = 2√2·ω`; `t = 1/(4 + ω² + r)`; `g = ω²·t`; `a1 = (8 - 2ω²)·t`; `a0 = (r - 4 - ω²)·t` | `Fil12But.cpp:104-114` |
| `filtDesign2ndButHighPass(ω, &g, &a1, &a0)` | `g(z²-2z+1)/(z² - a1·z - a0)` | same denominator; `g = 4·t` | `Fil12But.cpp:130-141` |

`filtDesignSimple1rstLowPass` matches the exact −3 dB point (comment `Fil12But.cpp:35`, "see MathCad
calculations") rather than using the bilinear approximation of `filtDesign1rstLowPass`.

### 11.2 Run functions — `FiltRun.cpp` (Direct Form I, negated denominators)

```c
// filtRun1rstLowPass   FiltRun.cpp:30-38
*out_old *= a0;
*out_old += (in + *in_old) * gain;
*in_old   = in;
*out      = *out_old;

// filtRun1rstHighPass  FiltRun.cpp:65-73   (same denominator, minus in numerator)
*out_old *= a0;
*out_old += (in - *in_old) * gain;
*in_old   = in;
*out      = *out_old;

// filtRun2ndLowPass    FiltRun.cpp:46-56
*out        = *out_m1 * a1 + *out_m2 * a0;
*out_m2     = *out_m1;
*out       += (in + 2.0*(*in_m1) + *in_m2) * gain;
*out_m1     = *out;
*in_m2      = *in_m1;
*in_m1      = in;

// filtRun2ndHighPass   FiltRun.cpp:82-93   (numerator sign flip only)
*out        = *out_m1 * a1 + *out_m2 * a0;
*out_m2     = *out_m1;
*out       += (in - 2.0*(*in_m1) + *in_m2) * gain;
*out_m1     = *out;
*in_m2      = *in_m1;
*in_m1      = in;
```

**These have no denormal bias.** They are also stateful via raw out-parameters — in Rust, make them
methods on a small struct holding `in_m1, in_m2, out_m1, out_m2`.

`struct filt2ndOrderButFilterType` (`filt.h:26-42`) bundles `gain, a1, a0` plus two channels' worth
of state (`out1_minus1/2`, `in1_minus1/2`, `out2_minus1/2`, `in2_minus1/2`).

---

## 12. The quantisation module (`Qnt`)

Purpose: precompute a **lookup table** from a 0..127 MIDI-style integer control position to a real
DSP value, so no `pow`/`log` runs on the audio thread. This is a 1990s design for a fixed-point DSP
board; in Rust you can usually replace the whole thing with a closure, but the *shapes* it encodes
are user-visible knob laws you must preserve.

### 12.1 Handle and modes

`u_qnt.h:30-62`:

```c
struct qntHdlType {
   CSlout *slout_hdl;  char msg1[1024];
   int in_out_mode;                               // one of the QNT_* modes below
   int array_size;                                // i_input_max - i_input_min + 1
   int min_int_input;
   int  *int_array;  long *long_array;  realtype *real_array;
   struct filt2ndOrderBoostCutShelfFilterType *filt_array;
   realtype r_input_min, r_input_max;
   int i_output_min, i_output_max;
   realtype r_output_min, r_output_max;
   long l_output_min, l_output_max;
   int output_quantized_flag, num_output_levels;
   int i_force_value_index;
   int levels_unequal_flag;
   realtype r_scale, r_scale_inv, half_delta;
};
```

| Mode | Value | Cite |
|---|---:|---|
| `QNT_INT_TO_INT` | 1 | `u_qnt.h:20` |
| `QNT_INT_TO_REAL` | 2 | `u_qnt.h:21` |
| `QNT_INT_TO_LONG` | 3 | `u_qnt.h:22` |
| `QNT_REAL_TO_INT` | 4 | `u_qnt.h:23` |
| `QNT_REAL_TO_REAL` | 5 | `u_qnt.h:24` |
| `QNT_REAL_TO_LONG` | 6 | `u_qnt.h:25` |
| `QNT_INT_TO_REAL_VOLUME` | 7 | `u_qnt.h:26` |
| `QNT_INT_TO_BOOST_CUT` | 8 | `u_qnt.h:27` |

`QNT_NUM_MIDI_STEPS = 128` (`qnt.h:22`); `MIDI_MIN_VALUE = 0`, `MIDI_MAX_VALUE = 127`
(`dsp/ptutil/include/midi.h:47, 59`).

### 12.2 Response-curve catalogue (`qntIToRInit`, `Qntitor.cpp:77-522`)

| Id | Name | Value | Law | Cite |
|---|---|---:|---|---|
| 0 | `QNT_RESPONSE_LINEAR` | 0 | `v[i] = out_min + i·(out_max-out_min)/(in_max-in_min)`, endpoint hard-set, then optional decimal snap | `qnt.h:27`; `Qntitor.cpp:117-206` |
| 1 | `QNT_RESPONSE_MIDI_VOLUME` | 1 | **squared** law: `v[i] = (i/(in_max-in_min))² · out_max`, `v[0]=0`, `v[last]=out_max` | `qnt.h:34`; `Qntitor.cpp:217-228` |
| 2 | `QNT_RESPONSE_MIDI_VOLUME_DISPLAY` | 2 | as above, then `v[i] = 20·log10(v[i])`; `v[0] = QNT_DB_VOLUME_OFF`; `v[last] = 20·log10(out_max)` | `qnt.h:39`; `Qntitor.cpp:231-246` |
| 3 | `QNT_RESPONSE_EXP` | 3 | `factor = (out_max/out_min)^(1/in_max)`; `v[0]=out_min`; `v[i]=v[i-1]·factor`; `v[last]=out_max`. Requires `out_min > 0` | `qnt.h:56`; `Qntitor.cpp:302-334` |
| 4 | `QNT_RESPONSE_TWO_PART_LINEAR` | 4 | snap-delta from `mthCalcQuantDelta`; upper segment `i·delta`; leftover points form a finer lower segment | `qnt.h:63`; `Qntitor.cpp:256-299` |
| 5 | `QNT_RESPONSE_EXP_FREQ` | 5 | delegates to `mthMidiOctaveFreqsPara` — puts points on 440-tuning note frequencies, endpoints 20 Hz / 20 kHz | `qnt.h:68`; `Qntitor.cpp:363-367` |
| 6 | `QNT_RESPONSE_Q_TYPE` | 6 | piecewise Q ladder, filled **from index 127 downward** — see below | `qnt.h:72`; `Qntitor.cpp:372-433` |
| 7 | `QNT_RESPONSE_EXP_FACTOR` | 7 | `up = (out_max/force)^(1/63)`, `down = (out_min/force)^(1/63)`; `v[63]=v[64]=force`; `v[64+i]=up^i`, `v[63-i]=down^i` | `qnt.h:77`; `Qntitor.cpp:340-357` |
| 8 | `QNT_RESPONSE_SQRT` | 8 | square the endpoints, interpolate linearly, take `sqrt` | `qnt.h:81`; `Qntitor.cpp:436-455` |
| 9 | `QNT_RESPONSE_LINEAR_NO_ROUND` | 9 | linear, skip the decimal snap; quantised variant uses `v -= fmod(v, delta)` | `qnt.h:84`; `Qntitor.cpp:183, 197-204` |
| 10-13 | `QNT_RESPONSE_MAXI_*` | 10-13 | maximiser boost ladder, see below | `qnt.h:89-92`; `Qntitor.cpp:458-518` |

Fixed values:

| Define | Value | Cite |
|---|---|---|
| `QNT_RESPONSE_MIDI_VOLUME_DB_MIN` | `-31.5` dB (comment: gives 0.25 dB steps over 128 points) | `qnt.h:45` |
| `QNT_DB_VOLUME_OFF` | `-1500.0` dB (stand-in for −∞) | `qnt.h:50` |

**`QNT_RESPONSE_Q_TYPE` ladder** (`Qntitor.cpp:381-432`) — filled backwards from index 127 to 0,
each segment continuing where the previous stopped:

| Segment | Start | Step | Run until |
|---|---|---|---|
| 1 | `0.2` | `0.025` | value `>= 1.0` |
| 2 | `1.0` | `0.05` | value `>= 1.5` |
| 3 | `1.5` | `0.1` | value `>= 5.0` |
| 4 | `5.0` | `0.2` | value `>= 10.0` |
| 5 | `10.0` | `0.5` | value `>= 20.0` |
| — | `q[0] = 20.0` hard set | | `Qntitor.cpp:432` |

The source warns at `Qntitor.cpp:378-380`: *"take care when changing these ranges, since if a prior
range fills all 128 points, next range will access bad memory"* — the `do/while` loops decrement
`index` and only test `index >= 0` **after** writing, so `q[-1]` is written when a segment exactly
fills the table. **This is an out-of-bounds write in the original.** Do not reproduce; bounds-check.

**`QNT_RESPONSE_MAXI_*` ladder** (`Qntitor.cpp:464-496`): `0 → 6` dB in `0.1` steps, `6 → 12` dB in
`0.2` steps, `12 → 30` dB in `0.5` steps, `v[0] = 0.0`, `v[127] = 30.0`, `v[126] = v[127]`
(`:496`). The `MAX_OUTPUT` variants then reverse-and-negate the array (`:498-509`); the `_DSP`
variants convert dB to linear with `10^(v/20)` (`:511-517`).

**Forced-value mode** (`Qntitor.cpp:130-164`): scans for the index whose linear value is closest to
`r_force_value`, then rebuilds the array with that value **repeated** at `force_value_index` and
`force_value_index + 1`, so a knob has a detent at (typically) 0.0 with a symmetric range around
it. `scale` is compensated by `-1` in the denominator to account for the repeated point (`:137`).

### 12.3 `QNT_INT_TO_BOOST_CUT` — the int→biquad table

`QntitoBoostCut.cpp:37-164`. This is the only Qnt mode that stores filter structs.

```
array_size   = i_input_max - i_input_min + 1
boost_factor = (r_boost_max - r_boost_min) / (i_input_max - i_input_min)

for i in 0..array_size:
    filt[i].r_center_freq = r_center_freq
    filt[i].r_samp_freq   = r_samp_freq
    filt[i].Q             = r_Q
    filt[i].boost         = r_boost_min + i * boost_factor        // LINEAR in dB
    if (i_filter_type == FILT_BOOST_CUT)  filtCalcParametric(&filt[i])
    else                                  filtCalcShelf(&filt[i], i_filter_type)
```

`qntIToBoostCutCalc(handle, i_input, out_filt)` (`:172-196`) is a plain array index
(`index = i_input - min_int_input`, bounds-checked) and a struct copy.

The `#if 0` block at `QntitoBoostCut.cpp:104-159` preserves an abandoned two-piece-linear boost
curve (0→3 dB over indices 0..51, 3→15 dB over 51..127) and **contains a syntax error**
(`=!=` at `:150`) proving it has never been compiled. Ignore it.

The single live caller is the Hyperbass / "Bass Boost" knob (`dfxpQnt.cpp:530-538`):

| Parameter | Value | Cite |
|---|---|---|
| input range | `0 .. 127` | `dfxpQnt.cpp:531`, `midi.h:47,59` |
| `DSP_PLY_BASSBOOST_MIN_VALUE` | `0.0` dB | `c_play.h:124` |
| `DSP_PLY_BASSBOOST_MAX_VALUE` | `15.0` dB | `c_play.h:125` |
| `DSP_PLY_BASSBOOST_CENTER_FREQ` | `90.0` Hz | `c_play.h:126` |
| `DSP_PLY_BASSBOOST_Q` | `2.5` | `c_play.h:127` |
| filter type | `FILT_BOOST_CUT` | `dfxpQnt.cpp:537` |

(Two earlier tunings are commented out at `c_play.h:111-114` — 12 dB / 100 Hz / Q 1.5 — and
`c_play.h:119-122` — 15 dB / 73.4 Hz / Q 2.5.) With `Q = 2.5` and `f0 = 90 Hz`, the low-frequency Q
limiter (§6.2 Rule A) does **not** fire (90 > 60), but the low-boost warp (Rule B) caps Q at
`0.2` for the first table entry and reaches `2.5` only once `boost >= (2.5-0.2)/3.3 = 0.697` dB,
i.e. from index `6` up.

`qntFreeUp` (`Qnt.cpp:35-79`) frees `filt_array` for this mode.

### 12.4 The simple scalar converters

| Function | Formula | Cite |
|---|---|---|
| `qntRToICalc` | `out = (int)((in - r_input_min)·r_scale + i_output_min + 0.5)` | `Qntrtoi.cpp:96-97` |
| `qntRToICalcFromOut` | `in = (out - i_output_min)·r_scale_inv + r_input_min` | `Qntrtoi.cpp:119-120` |
| `qntRToLCalc` | `out = (long)((in - r_input_min)·r_scale + l_output_min + 0.5)` | `Qntrtol.cpp:89-90` |
| `qntRToRCalc` | `out = (in - r_input_min)·r_scale + r_output_min` | `Qntrtor.cpp:104-105` |
| `qntIToRCalc` | table lookup, bounds-checked | `Qntitor2.cpp:559-581` |
| `qntIToLCalc` | table lookup, bounds-checked | `Qntitol.cpp:182-205` |
| `qntIToRCalcFromOut` | **linear scan** for the largest table value `<= r_output`; assumes ascending order | `Qntitor2.cpp:614-654` |

`r_scale = (out_max - out_min)/(in_max - in_min)`, `r_scale_inv = 1/r_scale` in all three
`RTo*` inits (`Qntrtoi.cpp:68-71`, `Qntrtol.cpp:61-64`, `Qntrtor.cpp:70-73`). Note the `+0.5`
rounding is applied *inside* the truncating cast, so negative outputs round toward zero incorrectly
— reproduce only if you need bit-exact knob positions.

### 12.5 `Qnt2But.cpp` — Butterworth coefficient tables

`qnt2ndOrderButterworthInit` (`Qnt2But.cpp:41-117`) builds **three** parallel `QNT_INT_TO_REAL`
tables (gain, a1, a0) for a 2nd-order Butterworth **highpass** swept exponentially between
`r_omega_min` and `r_omega_max`:

```
factor = (omega_max / omega_min) ^ (1 / i_input_max)         // note: i_input_max, not (max-min)
seed each of the 3 handles via qntIToRInit(..., 1.0, 10.0, ..., QNT_RESPONSE_EXP)
hard-set index 0        from filtDesign2ndButHighPass(omega_min)
hard-set index num-1    from filtDesign2ndButHighPass(omega_max)
for i in 1..num-2:  omega *= factor;  filtDesign2ndButHighPass(omega) -> gain/a1/a0
```

The `1.0, 10.0` output range passed to `qntIToRInit` is a throwaway — the arrays are fully
overwritten. It exists only because `QNT_RESPONSE_EXP` rejects `out_min <= 0`
(`Qntitor.cpp:306-307`).

`qntIToRSimpleLowpassInit` (`Qnt2But.cpp:130-184`) derives a coefficient table from an existing
frequency table:

```
freq  = freq_table[index] * r_scale_freq
omega = TWO_PI * freq * r_sampling_period
filtDesignSimple1rstLowPass(omega, &coeff)
```

### 12.6 `Qntitor2.cpp` — derived tables

| Function | Law | Cite |
|---|---|---|
| `qntIToRInitReverbFeedback` | `v[i] = sign(ref)·|ref|^(element_delay/reference_delay)` where `ref` is a linear ramp over `[out_min, out_max]` | `Qntitor2.cpp:74-87` |
| `qntIToRInitTrackPitchIToR` | cents → timebase decrement: `a = 2^(cents/1200)`; `k = a - 1`; `v[i] = -k` | `Qntitor2.cpp:143-154` |
| `qntIToRInitPitchCompIToR` | `v[i] = 1.0 - ref[i]` ("EMPIRICAL FIRST SHOT", `:207`) | `Qntitor2.cpp:209-216` |
| `qntIToRInitPitchSpliceDelay` | 25-way `if/else if` table of (splice, delay) pairs keyed on exact float equality against cents values `-1200 .. +1200` in steps of 100; fall-through `(0.76, 2300.0)` | `Qntitor2.cpp:284-425` |
| `qntIToRdBCalcInit` | `v[i] = (db > threshold) ? 10^(db/20) : 0.0` | `Qntitor2.cpp:478-488` |
| `qntIToRTimeConstantBeta` | `v[i] = exp( -1 / (time_constant_ms · 0.001 · fs) )` | `Qntitor2.cpp:539-547` |

`qntIToRInitPitchSpliceDelay`'s float `==` comparisons against `-1200.0f`, `-1100.0f`, … only work
because the source table was generated by the same float arithmetic. In Rust, index by an integer
semitone count instead.

### 12.7 Decimal snap helpers (`mth`)

Declared `mth.h:40-42`, implemented `audiopassthru/src/MTH/MthUtil.cpp`.

**`mthCalcQuantDelta(range_min, range_max, num_levels, &delta)`** (`MthUtil.cpp:190-235`)
Picks a "nice" step from the `{1, 2, 2.5, 5, 10} × 10^k` series:

```
rough  = (range_max - range_min) / (num_levels - 1)
lg     = log10(rough)
ten_p  = (int)lg ;  rem = lg - ten_p
if (lg < 0) { ten_p = (int)lg - 1 ;  rem += 1.0 }        // force rem >= 0
if (rem == 0) delta = rough                              // rough is an exact power of ten
else {
    delta = 10.0
    if (log10(5.0) > rem) delta = 5.0                    // log10(5)   ≈ 0.69897
    if (log10(2.5) > rem) delta = 2.5                    // log10(2.5) ≈ 0.39794
    if (log10(2.0) > rem) delta = 2.0                    // log10(2)   ≈ 0.30103
    multiply/divide by 10 `|ten_p|` times
}
```

Note the ladder never yields `1.0` for a non-zero remainder — the smallest mantissa is `2.0`.

**`mthCalcRoundedValue(v, delta, out_min, out_max)`** (`MthUtil.cpp:243-264`) — round-half-away-
from-zero to a multiple of `delta`, then clamp:

```
t = v / delta
t += (t >= 0) ? 0.5 : -0.5
t  = (long)t * delta
clamp t to [out_min, out_max]
```

**`mthCalcClosestNiceValue(v, num_places)`** (`MthUtil.cpp:275-294`) — rounds to a fractional
number of significant decimal places, e.g. `num_places = 2.5` rounds to the `…,100,105,110,…`
series:

```
n_dec   = (int)log10(v) + 1
rem     = num_places - (int)num_places
lg_r    = n_dec - (int)num_places + log10(rem)
round_v = 10 ^ lg_r
r       = (int)(v/round_v) * round_v
if (fmod(v, round_v) >= round_v*0.5)  r += round_v
```

`half_delta` is stashed on the handle (`Qntitor.cpp:189, 271`) and read back by
`qntIToRGetHalfDelta` (`Qntitor2.cpp:588-603`) for hit-testing knob detents.

---

## 13. Golden test vectors

Produced by compiling `filtCalcParametric` + `filtBW2ANGLE` verbatim with `realtype = float`,
`biqdRealtype = float` and glibc libm on x86-64. **(computed)** Use these as regression fixtures for
the Rust port; agreement to ~1e-6 relative is the right target for an `f32` reimplementation.

**fs = 48000 Hz, nominal Q = 4.33336544 (31-band, multiplier 1)**

| f0 (Hz) | boost (dB) | Q actually used | b0 | b1 = a1 | b2 | a2 |
|---:|---:|---:|---:|---:|---:|---:|
| 20 | +3 | 1.000000 | 1.00045383 | −1.99779272 | 0.997345746 | 0.997799635 |
| 20 | −3 | 1.000000 | 0.999546349 | −1.99688637 | 0.997346997 | 0.996893346 |
| 20 | +6 | 1.000000 | 1.00091815 | −1.99814796 | 0.997236729 | 0.998154879 |
| 20 | +12 | 1.000000 | 1.00364327 | −1.99754894 | 0.993912518 | 0.997555792 |
| 20 | −12 | 1.000000 | 0.996369958 | −1.99029768 | 0.993934631 | 0.990304589 |
| 100 | +3 | 4.333365 | 1.00052357 | −1.99729049 | 0.99693799 | 0.997461617 |
| 100 | −3 | 4.333365 | 0.999476612 | −1.99624515 | 0.9969396 | 0.996416271 |
| 100 | +6 | 4.333365 | 1.00105929 | −1.9977001 | 0.996812046 | 0.997871339 |
| 100 | +12 | 4.333365 | 1.00420296 | −1.99700928 | 0.992977381 | 0.997180343 |
| 100 | −12 | 4.333365 | 0.995814681 | −1.98865092 | 0.993006706 | 0.988821387 |
| 1000 | +3 | 4.333365 | 1.00517726 | −1.95800531 | 0.969723582 | 0.974900723 |
| 1000 | −3 | 4.333365 | 0.994849443 | −1.94792044 | 0.969879448 | 0.964728892 |
| 1000 | +6 | 4.333365 | 1.01049304 | −1.96198428 | 0.968420982 | 0.978914022 |
| 1000 | +12 | 4.333365 | 1.04150569 | −1.95528173 | 0.930648088 | 0.972153723 |
| 1000 | −12 | 4.333365 | 0.960148394 | −1.8773607 | 0.933411896 | 0.89356029 |
| 10000 | +3 | 4.333365 | 1.04683125 | −0.458875895 | 0.726128995 | 0.772960186 |
| 10000 | −3 | 4.333365 | 0.955263793 | −0.438347548 | 0.738380909 | 0.693644702 |
| 10000 | +6 | 4.333365 | 1.0964942 | −0.467451334 | 0.709598899 | 0.806093097 |
| 10000 | +12 | 4.333365 | 1.37129164 | −0.453166455 | 0.379608899 | 0.750900507 |
| 10000 | −12 | 4.333365 | 0.729239404 | −0.330466837 | 0.547586262 | 0.276825756 |
| 20000 | +3 | 4.333365 | 1.08569419 | 1.3722614 | 0.498856902 | 0.584551036 |
| 20000 | −3 | 4.333365 | 0.921069682 | 1.2639482 | 0.538412094 | 0.459481835 |
| 20000 | +6 | 4.333365 | 1.17935133 | 1.4199264 | 0.460238516 | 0.639589846 |
| 20000 | +12 | 4.333365 | 1.67246497 | 1.34133768 | −0.12362162 | 0.548843443 |
| 20000 | −12 | 4.333365 | 0.597919881 | 0.802012444 | 0.328164369 | −0.0739158168 |

Observe: the 20 Hz rows all show `Q = 1.000000` — Rule A of §6.2 firing exactly at the boundary
(`(20-20)*0.475 + 1.0 = 1.0`). If your port shows `4.33` there, you skipped the limiter.

**fs = 44100 Hz, 10-band table, Q = 1.59764123, boost = +6 dB**

| f0 (Hz) | b0 | b1 = a1 | b2 | a2 |
|---:|---:|---:|---:|---:|
| 62.500 | 1.00195277 | −1.99599671 | 0.994123161 | 0.996075928 |
| 115.734 | 1.0036099 | −1.99247456 | 0.989135563 | 0.992745578 |
| 214.311 | 1.0066644 | −1.98568177 | 0.979943454 | 0.98660779 |
| 396.850 | 1.01227164 | −1.97218323 | 0.963068187 | 0.97533983 |
| 734.867 | 1.02249348 | −1.94409418 | 0.932305515 | 0.954798996 |
| 1360.790 | 1.04089975 | −1.88187945 | 0.876911581 | 0.917811275 |
| 2519.840 | 1.07337773 | −1.73443282 | 0.77916801 | 0.852545798 |
| 4666.120 | 1.12900734 | −1.37003541 | 0.61174953 | 0.740756929 |
| 8640.480 | 1.2213887 | −0.518224478 | 0.333726466 | 0.555115104 |
| 16000.000 | 1.37729919 | 0.808339179 | −0.135489658 | 0.241809562 |

---

## 14. Preset band-count interpolation

`DfxDspPrivate::getGraphicEqInfoFromVals` (`DfxDspEq.cpp:127-247`). When a preset's band count
`nBands` differs from the live count `graphic_eq_num_bands`:

**Upsampling** (`nBands < N`, linear interpolation, `DfxDspEq.cpp:193-209`):

```
for i in 1..=N:
    src   = 1.0 + (i-1) * (nBands - 1) / (N - 1)
    lo    = (int)src ;  hi = lo + 1 ;  frac = src - lo
    if (lo >= 1 && hi <= nBands)  out[i] = g[lo] + (g[hi] - g[lo]) * frac
    else if (lo == nBands)        out[i] = g[lo]
```

Note: when neither branch matches, `out[i]` is **left uninitialised** (the array is a stack local,
`DfxDspEq.cpp:180`). With `i = N`, `src = nBands` exactly, so `lo = nBands`, `hi = nBands+1` — the
first `if` fails and the `else if` catches it. But `src` is a `float` (`realtype`) computed from a
division, so if it lands at `nBands - 1e-7` then `lo = nBands-1`, `hi = nBands`, and it works; if it
lands at `nBands + 1e-7` it still hits the `else if`. It is safe by luck, not construction. Use
`clamp` in Rust.

**Downsampling** (`nBands > N`, nearest selection, `DfxDspEq.cpp:213-218`):

```
for i in 1..=N:
    src = 1 + (int)( (i-1)*(nBands-1)/(N-1) + 0.5 )
    out[i] = g[src]
```

**Matched count** (`DfxDspEq.cpp:229-241`): gains applied directly **and** the preset's band centre
frequencies applied via `GraphicEqSetBandFreq`.

The scratch arrays are `realtype r_boost_cut_original[35]` / `r_boost_cut_interpolated[35]`, indexed
1-based, with the comment "Dimension arrays more than max bands number (31)"
(`DfxDspEq.cpp:178-180`). A preset claiming 35+ bands overflows. Bounds-check in Rust.

The same remap logic appears again, independently, in `GraphicEqSetNumBands`
(`GraphicEqSet.cpp:207-245`) for live band-count changes.

Empty-EQ presets (`hp_graphicEq == NULL`) turn the EQ **on** and flatten every band
(`DfxDspEq.cpp:144-158`).

---

## 15. `resetEQ` and on/off

* `resetEQ()` — sets every band (1..`DFXP_GRAPHIC_EQ_NUM_BANDS`) to `0.0` in both memory and
  registry (`DfxDspEq.cpp:109-122`).
* `eqOn(bool)` — `eqSetProcessingOn(DFXP_STORAGE_TYPE_ALL, IS_TRUE/IS_FALSE)`
  (`DfxDspEq.cpp:335-347`).
* Turning the EQ **off** also forces the Hyperbass button off
  (`DFX_UI_BUTTON_BASS_BOOST → IS_FALSE`, `DfxDspEq.cpp:73-78`) — but only on the
  registry-change-detection path, not from `eqOn()`.
* `getEqBandFrequency` / `setEqBandFrequency` / `getEqBandBoostCut` / `setEqBandBoostCut` take a
  **0-based** band index and add 1 before calling down (`DfxDspEq.cpp:463, 476, 488, 499, 508`).
  Everything below `DfxDspPrivate` is 1-based. Pick one convention in Rust and stick to it.

---

## 16. Windows-specific machinery → Linux equivalents

| Windows mechanism | What it achieves | Cite | Linux / Rust substitute |
|---|---|---|---|
| `#define realtype float`, `biqdRealtype = realtype` | fixes the whole design path to `f32` | `codedefs.h:150`, `filt.h:24` | `type Real = f32;` — keep it (see §17) |
| `PT_DECLSPEC` = `__declspec(dllexport/dllimport)` | DLL symbol visibility | `codedefs.h:59-71` | nothing; use a normal Rust crate. If you need a C ABI, `#[no_mangle] pub extern "C"` |
| `#include <windows.h>` / `<crtdbg.h>` unconditionally in `codedefs.h` | CRT debug heap + Win32 types | `codedefs.h:35-38` | delete; the `__ANDROID__` guard already shows the intended non-Windows path |
| `NOT_OKAY` expands to `MessageBoxW(...)` + `DebugBreak()` — **including in release builds** | error reporting | `codedefs.h:98-141` | `Result<(), EqError>`. A modal dialog from an audio callback is a hard deadlock on any platform; this is one of the worst things in the codebase |
| `HWND` → `void*` fallback | non-Windows builds | `codedefs.h:156-160` | n/a |
| `regCreateKey_Wide` / `regReadKey_Wide` under `HKEY_CURRENT_USER\...\LastUsed\EQ\` | per-band gain and `EQ_ON` persistence | `DfxDspEq.cpp:269-281, 306-316`; `dfxpEq.cpp:122-134, 263-278, 349-360` | a single TOML/JSON file under `$XDG_CONFIG_HOME/fxsound/` (fall back to `~/.config/fxsound/`). Use `directories`/`etcetera` + `serde`. Write atomically (tmp + `rename`) — the registry gave you that for free |
| `wchar_t` (UTF-16) keys and values, `swprintf(L"%.2f")`, `wcscmp`, `_wtoi`, `swscanf(L"%g")` | float↔string round-trip and change detection | `DfxDspEq.cpp:53-54, 95`; `dfxpEq.cpp:274, 368` | Rust `String` is UTF-8 natively. Keep the 2-decimal rounding if you want preset-for-preset gain parity, but compare `f32`s with an epsilon, not strings. Never use locale-dependent formatting — `format!("{:.2}", x)` is locale-independent in Rust, which is *better* than the original |
| Registry polling loop (`update_from_registry_` flag + `eqUpdateFromRegistry`) | cross-process settings sync | `DfxDspEq.cpp:48-107, 113, 337, 454, 472, 506` | `inotify` on the config file via `notify`, or skip it entirely — a single-process Rust app does not need IPC for its own settings. If you keep a CLI/DBus control surface, use a `crossbeam` SPSC channel into the audio thread instead |
| Mutable global `int DFXP_GRAPHIC_EQ_NUM_BANDS` written from `GraphicEqSetNumBands` | band count visible to `dfxp` layer | `DfxDspEq.cpp:32`, `GraphicEqSet.cpp:154`, `dfxpDefs.h:263` | a field on the EQ struct; the audio thread reads it through the same lock-free handoff as the coefficients |
| `calloc`/`free` of section arrays, `PT_HANDLE` = `typedef int` cast to/from pointers | opaque handles | `codedefs.h:144`, `Sos.cpp:45-95` | `Box<Sos>` / `Vec<Biquad>`. `typedef int PT_HANDLE` with pointer casts is UB on LP64 and only compiles because every use round-trips through `PT_HANDLE*` |
| MSVC x86-64 default FP env: SSE2, **no FTZ, no DAZ**, `/fp:precise` | denormals are computed, slowly | build settings + `u_sos.h:41` | Rust on x86-64 Linux has the same default (MXCSR FTZ/DAZ clear). The `1e-30` bias therefore behaves identically. See §18 for whether to additionally set FTZ |
| Win32 `MessageBox` in `ptDebugNotOkay`/`ptReleaseNotOkay`; `IsDebuggerPresent`, `DebugBreak` | dev diagnostics | `codedefs.h:100-135` | `tracing::error!` + return `Err` |
| Buffer-length stats written to the registry **on every audio buffer** (`dfxp_UpdateBufferLengthInfo`) | telemetry | `dfxpProcessReal.cpp:101` | delete, or push to a lock-free ring the UI thread drains. Never touch the filesystem from the PipeWire `on_process` callback |
| `DAW_MAX_BUFFER_SIZE` silent pass-through for oversized buffers | crash guard for fixed-size scratch arrays | `dfxpProcessReal.cpp:109-110` | process in chunks, or size buffers dynamically. PipeWire quantum can be large (up to 8192 frames by default policy) |

### 16.1 PipeWire integration notes for this subsystem

* The EQ is pure per-sample DSP with no latency and no lookahead, so it maps directly onto a
  PipeWire **filter node** (`pipewire::filter::Filter`, or `pw-filter` via the `pipewire` crate) with
  one input and one output port, `F32` planar or interleaved.
  The original works on **interleaved** `f32` (`SosProcess.cpp:639`, `k += i_num_channels`).
  PipeWire negotiates `SPA_AUDIO_FORMAT_F32P` (planar) by default for filters — either request
  interleaved `F32` or de-interleave; deinterleaved is actually a better fit for the cascade since
  each channel has its own state.
* `GraphicEqProcess`'s "sample rate changed → redesign coefficients inline" (§2) must move off the
  realtime path. On PipeWire the rate is known from `param_changed` / the `spa_io_position`
  callback, which runs on the data thread. Redesign in a worker and publish with a triple-buffer or
  `arc-swap`.
* A `pipewire` *filter-chain* module (`libpipewire-module-filter-chain` with `bq_peaking` nodes)
  would give you a 31-band EQ for free, but its `bq_peaking` uses the RBJ cookbook design, **not**
  this one — different Q interpretation, no low-frequency/low-boost Q limiters, no `f32`
  cancellation artefacts. It will not sound the same. Implement the DSP in Rust.
* Denormals: set FTZ/DAZ once per audio thread if you drop the `1e-30` bias
  (`core::arch::x86_64::_MM_SET_FLUSH_ZERO_MODE(_MM_FLUSH_ZERO_ON)` and
  `_MM_SET_DENORMALS_ZERO_MODE(_MM_DENORMALS_ZERO_ON)`), but be aware PipeWire's data thread is
  shared and MXCSR is per-thread, so do it inside your `on_process` prologue or accept the bias.
  On aarch64 the equivalent is the `FZ` bit of `FPCR`.
* Everything in this subsystem is allocation-free once built. Keep it that way: preallocate
  `Vec<Biquad>` to 32 (`SOS_MAX_NUM_SOS_SECTIONS`, `sos.h:27`) and never resize on the audio thread.

---

## 17. Rust code sketches

Types chosen to match the original exactly: `f32` for everything the original calls `realtype`,
`f64` only where the original explicitly uses `double` (the band-frequency/Q derivation and
`filtCalcFirResponse`).

```rust
//! 09-dsp-eq: FxSound graphic EQ, faithful port.
//!
//! `Real` mirrors `#define realtype float` (codedefs.h:150).
//! `BiqdReal` mirrors `#define biqdRealtype realtype` (filt.h:24) -- yes, also f32.
pub type Real = f32;
pub type BiqdReal = f32;

pub const PI_D: f64 = 3.141592653589793238462643;      // FiltCalcBiqd.cpp:28
pub const ROOT2O2: f64 = 0.7071067811965475244;        // FiltCalcBiqd.cpp:29 (sic)
pub const SPN: f64 = 1.65436e-24;                      // FiltCalcBiqd.cpp:30

pub const SOS_FLOAT_BIAS: Real = 1.0e-30;              // u_sos.h:41
pub const SOS_MAX_SECTIONS: usize = 32;                // sos.h:27

// FiltCalcBiqd.cpp:146-155
const FILT_Q_UPPER_LIMIT_FREQ: Real = 60.0;
const FILT_Q_LOWER_LIMIT_FREQ: Real = 20.0;
const FILT_Q_UPPER_LIMIT: Real = 20.0;
const FILT_Q_LOWER_LIMIT: Real = 1.0;
const FILT_Q_LIMIT_SCALE: Real =
    (FILT_Q_UPPER_LIMIT - FILT_Q_LOWER_LIMIT) / (FILT_Q_UPPER_LIMIT_FREQ - FILT_Q_LOWER_LIMIT_FREQ); // 0.475
const FILT_BOOST_WARP_LEVEL: Real = 6.0;
const FILT_BOOST_MAX_Q: Real = 20.0;
const FILT_BOOST_MIN_Q: Real = 0.2;
const FILT_BOOST_SCALE: Real = (FILT_BOOST_MAX_Q - FILT_BOOST_MIN_Q) / FILT_BOOST_WARP_LEVEL; // 3.3

// GraphicEq.h:46 / dfxpDefs.h:264-265
pub const GRAPHIC_EQ_MAX_BOOST_OR_CUT_DB: Real = 20.0;
pub const DFXP_EQ_MIN_BOOST_OR_CUT_DB: Real = -12.0;
pub const DFXP_EQ_MAX_BOOST_OR_CUT_DB: Real = 12.0;
```

### 17.1 `filtBW2ANGLE`

```rust
/// Port of filtBW2ANGLE (FiltCalcBiqd.cpp:55-82).
/// `a` is the bilinear warp factor; `bandwidth` is normalised (cycles/sample).
/// Returns the prototype bandedge as a normalised frequency.
fn bw_to_angle(a: BiqdReal, bandwidth: BiqdReal) -> BiqdReal {
    // Every libm call in the original goes through double and lands back in f32.
    let t   = (2.0 * PI_D * bandwidth as f64).tan() as BiqdReal;
    let a2  = a * a;
    let a4  = a2 * a2;
    let mut d = 2.0 * a2 * t;
    let sn  = (1.0 + a4) * t;
    let cs  = 1.0 - a4;
    let mag = ((sn as f64 * sn as f64 + cs as f64 * cs as f64).sqrt()) as BiqdReal;
    d /= mag;
    let delta = (sn as f64).atan2(cs as f64) as BiqdReal;

    // Guard: mathematically |d| <= 1, but f32 rounding can overshoot.
    // The original does NOT clamp and will produce NaN. We clamp; document the divergence.
    let asnd = (d.clamp(-1.0, 1.0) as f64).asin() as BiqdReal;

    let mut theta = 0.5 * (PI_D as BiqdReal - asnd - delta);
    let tmp = 0.5 * (asnd - delta);
    if tmp > 0.0 && tmp < theta {
        theta = tmp;                       // take the principal branch
    }
    if bandwidth >= 0.5 {
        theta = 0.005;                     // the documented HACK, FiltCalcBiqd.cpp:78-79
    }
    theta / (2.0 * PI_D as BiqdReal)
}
```

### 17.2 Parametric peaking design

```rust
#[derive(Clone, Copy, Debug, Default)]
pub struct BiquadCoeffs {
    pub b0: Real,
    pub b1: Real,   // == a1 for the parametric design
    pub b2: Real,
    pub a1: Real,
    pub a2: Real,
    pub on: bool,
}

/// Port of filtCalcParametric (FiltCalcBiqd.cpp:109-222).
/// `q_in` is taken by value: the original mutates only its local copy
/// (filtSosParametric passes Q by value, FiltbiqdSos.cpp:58), so the
/// GraphicEq handle's Q survives untouched.
pub fn calc_parametric(fs: Real, f0: Real, boost_db: Real, q_in: Real) -> BiquadCoeffs {
    // Step 0: exact zero -> bypass section (FiltCalcBiqd.cpp:132-137)
    if boost_db == 0.0 {
        return BiquadCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, on: false };
    }

    let mut q = q_in;

    // Rule A: low-frequency Q limit (FiltCalcBiqd.cpp:160-166).
    // NOTE: goes negative below 20 Hz in the original. See the risks section.
    if f0 < FILT_Q_UPPER_LIMIT_FREQ {
        let max_q = (f0 - FILT_Q_LOWER_LIMIT_FREQ) * FILT_Q_LIMIT_SCALE + FILT_Q_LOWER_LIMIT;
        if q > max_q { q = max_q; }
    }
    // Rule B: low-boost Q warp (FiltCalcBiqd.cpp:168-176)
    let abs_boost = boost_db.abs();
    if abs_boost < FILT_BOOST_WARP_LEVEL {
        let max_q = abs_boost * FILT_BOOST_SCALE + FILT_BOOST_MIN_Q;
        if q > max_q { q = max_q; }
    }

    let w0: BiqdReal = f0 / fs;              // normalised centre, cycles/sample
    let bandwidth: BiqdReal = w0 / q;        // normalised bandwidth

    // Bilinear warp factor: prototype is centred at 0.25 (= fs/4).
    let a   = ((PI_D * (w0 as f64 - 0.25)).tan()) as BiqdReal;
    let asq = a * a;

    // dB -> linear, and the bandedge reference level.
    let big_a = (10.0f64.powf(boost_db as f64 / 20.0)) as BiqdReal;
    let f_ref: BiqdReal = if boost_db < 6.0 && boost_db > -6.0 {
        (big_a as f64).sqrt() as BiqdReal            // half-boost bandedge
    } else if big_a > 1.0 {
        big_a / (2.0f64.sqrt() as BiqdReal)          // -3 dB from the boost
    } else {
        big_a * (2.0f64.sqrt() as BiqdReal)          // +3 dB from the cut
    };

    let xfmbw = bw_to_angle(a, bandwidth);
    let c = (1.0 / (2.0 * PI_D * xfmbw as f64).tan()) as BiqdReal;   // cotangent

    let f2  = f_ref * f_ref;
    let tmp = big_a * big_a - f2;
    let alphad = if (tmp as f64).abs() <= SPN {
        c
    } else {
        (((c as f64 * c as f64) * (f2 as f64 - 1.0) / tmp as f64).sqrt()) as BiqdReal
    };
    let alphan = big_a * alphad;

    let a2plus1  = 1.0 + asq;
    let ma2plus1 = 1.0 - asq;

    let b0_raw = a2plus1 + alphan * ma2plus1;
    let b1_raw = 4.0 * a;
    let b2_raw = a2plus1 - alphan * ma2plus1;
    let a0_raw = a2plus1 + alphad * ma2plus1;
    let a2_raw = a2plus1 - alphad * ma2plus1;

    let recip = 1.0 / a0_raw;
    let b0 = b0_raw * recip;
    let b1 = b1_raw * recip;
    let b2 = b2_raw * recip;
    let a2 = a2_raw * recip;

    BiquadCoeffs { b0, b1, b2, a1: b1, a2, on: true }   // a1 = b1, FiltCalcBiqd.cpp:217
}
```

### 17.3 The section and its inner loop

```rust
/// One cascaded second-order section. Mirrors `struct sosSectionType` (u_sos.h:47-62)
/// minus the never-read `a1_old`/`a2_old`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Section {
    pub c: BiquadCoeffs,
    /// Direct-Form-II-Transposed state, one pair per channel.
    pub s1: [Real; 8],
    pub s2: [Real; 8],
}

impl Section {
    /// Exact port of the three lines at SosProcess.cpp:576-578.
    /// Relies on c.a1 == c.b1 (true for every parametric section; NOT true for shelves).
    #[inline(always)]
    pub fn tick(&mut self, ch: usize, x: Real) -> Real {
        let y = self.s1[ch] + self.c.b0 * x + SOS_FLOAT_BIAS;
        self.s1[ch] = (x - y) * self.c.b1 + self.s2[ch];
        self.s2[ch] = self.c.b2 * x - self.c.a2 * y;
        y
    }

    /// General DF2T -- use this if you ever install a shelf section.
    #[inline(always)]
    pub fn tick_general(&mut self, ch: usize, x: Real) -> Real {
        let y = self.s1[ch] + self.c.b0 * x + SOS_FLOAT_BIAS;
        self.s1[ch] = self.c.b1 * x - self.c.a1 * y + self.s2[ch];
        self.s2[ch] = self.c.b2 * x - self.c.a2 * y;
        y
    }

    #[inline]
    pub fn reset(&mut self) { self.s1 = [0.0; 8]; self.s2 = [0.0; 8]; }
}

pub struct Sos {
    pub sections: Vec<Section>,     // capacity SOS_MAX_SECTIONS, never reallocated hot
    pub num_active: usize,
    pub master_gain: Real,          // linear
    pub balance_left: Real,
    pub balance_right: Real,
}

impl Sos {
    /// Interleaved stereo, in-place safe. Port of SosProcess.cpp:585-637.
    pub fn process_stereo(&mut self, buf: &mut [Real]) {
        for frame in buf.chunks_exact_mut(2) {
            let (mut l, mut r) = (frame[0], frame[1]);
            for s in &mut self.sections[..self.num_active] {
                if !s.c.on { continue; }        // off sections are SKIPPED, state left stale
                l = s.tick(0, l);
                r = s.tick(1, r);
            }
            frame[0] = l * self.master_gain * self.balance_left;
            frame[1] = r * self.master_gain * self.balance_right;
        }
    }
}
```

### 17.4 Band layout and Q

```rust
/// GraphicEqSet.cpp:430-492 -- the hard-coded tables.
pub fn band_table(n: usize) -> Option<(&'static [Real], Real, Real)> {
    // (frequencies, min_band_freq, max_band_freq)
    const F5:  [Real; 5]  = [62.5, 250.0, 1000.0, 4000.0, 16000.0];
    const F10: [Real; 10] = [62.5, 115.734, 214.311, 396.85, 734.867,
                             1360.79, 2519.84, 4666.12, 8640.48, 16000.0];
    const F15: [Real; 15] = [25.0, 40.0, 63.0, 100.0, 160.0, 250.0, 400.0, 630.0,
                             1000.0, 1600.0, 2500.0, 4000.0, 6300.0, 10000.0, 16000.0];
    const F20: [Real; 20] = [20.0, 31.5, 40.0, 63.0, 80.0, 125.0, 160.0, 250.0, 315.0, 500.0,
                             630.0, 1000.0, 1250.0, 2000.0, 2500.0, 4000.0, 5000.0, 8000.0,
                             10000.0, 16000.0];
    const F31: [Real; 31] = [20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0,
                             200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0,
                             1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0,
                             10000.0, 12500.0, 16000.0, 20000.0];
    match n {
        5  => Some((&F5,  62.5, 16000.0)),
        10 => Some((&F10, 62.5, 16000.0)),
        15 => Some((&F15, 25.0, 16000.0)),
        20 => Some((&F20, 20.0, 16000.0)),
        31 => Some((&F31, 20.0, 20000.0)),
        _  => None,
    }
}

/// Generic geometric fallback, GraphicEqSet.cpp:495-508. Computed in f64, stored as f32.
pub fn band_freqs_geometric(n: usize, min_hz: f64, max_hz: f64) -> Vec<Real> {
    let ratio = max_hz / min_hz;
    (0..n)
        .map(|i| (min_hz * ratio.powf(i as f64 / (n as f64 - 1.0))) as Real)
        .collect()
}

/// GraphicEqSet.cpp:512-525. f64 maths, f32 result.
pub fn derive_q(min_hz: f64, max_hz: f64, n: usize, q_multiplier: Real) -> Real {
    if n == 1 { return 1.0; }                              // GraphicEqSet.cpp:388
    let r = (max_hz / min_hz).powf(1.0 / (n as f64 - 1.0));
    let mut q = (r.sqrt() / (r - 1.0)) as Real;
    q *= q_multiplier;
    if q < 1.0 { q = 1.0; }                                // GraphicEqSet.cpp:524
    q
}
```

### 17.5 Applying a band gain (the clamp chain)

```rust
/// Port of GraphicEqSetBandBoostCut (GraphicEqSet.cpp:258-313), 0-based band index here.
pub fn set_band_boost(
    sections: &mut [Section],
    stored_boost: &mut [Real],
    centre_freq: &[Real],
    band: usize,
    mut boost_db: Real,
    fs: Real,
    q: Real,
) {
    let f0 = centre_freq[band];

    // Bypass: exact zero, or the band is at/above Nyquist.  GraphicEqSet.cpp:289
    if boost_db == 0.0 || f0 * 2.0 >= fs {
        sections[band].c = BiquadCoeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, on: false };
        stored_boost[band] = 0.0;
        return;
    }

    boost_db = boost_db.clamp(-GRAPHIC_EQ_MAX_BOOST_OR_CUT_DB, GRAPHIC_EQ_MAX_BOOST_OR_CUT_DB);

    // The original's exact-float-equality early-out.  GraphicEqSet.cpp:305
    if boost_db == stored_boost[band] { return; }

    sections[band].c = calc_parametric(fs, f0, boost_db, q);
    stored_boost[band] = boost_db;
}
```

### 17.6 Response evaluation for the UI curve

```rust
/// filtPolyCalc2ndOrderResponse (Filtpoly.cpp:30-40). `f` is normalised (cycles/sample).
#[inline]
fn poly2_mag(c0: Real, c1: Real, c2: Real, f: Real) -> Real {
    const TWO_PI: Real = 6.2831853072;      // mth.h:19
    const FOUR_PI: Real = 12.5663706144;    // mth.h:20
    let w = TWO_PI * f;
    let w2 = FOUR_PI * f;
    let re = c0 * w2.cos() + c1 * w.cos() + c2;
    let im = c0 * w2.sin() + c1 * w.sin();
    (re * re + im * im).sqrt()
}

/// filtPolyCalcBiquadResponseFiltStruct (Filtpoly.cpp:83-92) -- denominator a0 forced to 1.
#[inline]
pub fn biquad_mag(c: &BiquadCoeffs, f_norm: Real) -> Real {
    poly2_mag(c.b0, c.b1, c.b2, f_norm) / poly2_mag(1.0, c.a1, c.a2, f_norm)
}

/// Whole-cascade magnitude in dB, for the egui curve.
pub fn cascade_db(sections: &[Section], f_hz: Real, fs: Real) -> Real {
    let f = f_hz / fs;
    let mut mag = 1.0;
    for s in sections {
        if s.c.on { mag *= biquad_mag(&s.c, f); }
    }
    20.0 * mag.log10()
}
```

### 17.7 Quantisation table (only if you need knob-law parity)

```rust
/// mthCalcQuantDelta (MthUtil.cpp:190-235): pick a "nice" step from {2, 2.5, 5, 10} x 10^k.
pub fn calc_quant_delta(range_min: Real, range_max: Real, num_levels: i32) -> Real {
    let rough = (range_max - range_min) / (num_levels - 1) as Real;
    let lg = (rough as f64).log10() as Real;
    let mut ten_p = lg as i32;
    let mut rem = lg - ten_p as Real;
    if lg < 0.0 { ten_p = lg as i32 - 1; rem += 1.0; }
    if rem == 0.0 { return rough; }

    let mut delta: Real = 10.0;
    if (5.0f64).log10() as Real > rem { delta = 5.0; }
    if (2.5f64).log10() as Real > rem { delta = 2.5; }
    if (2.0f64).log10() as Real > rem { delta = 2.0; }
    // The original multiplies/divides in a loop -- do the same, powf would differ in f32.
    for _ in 0..ten_p.max(0)      { delta *= 10.0; }
    for _ in 0..(-ten_p).max(0)   { delta *= 0.1;  }
    delta
}

/// mthCalcRoundedValue (MthUtil.cpp:243-264): round half away from zero, then clamp.
pub fn calc_rounded_value(v: Real, delta: Real, out_min: Real, out_max: Real) -> Real {
    let mut t = v / delta;
    t += if t >= 0.0 { 0.5 } else { -0.5 };
    ((t as i64) as Real * delta).clamp(out_min, out_max)
}
```

---

## 18. Behaviour traps you must decide about explicitly

1. **`Q` can go negative below 20 Hz.** Rule A (§6.2) is `maxQ = (f0 - 20)*0.475 + 1.0` with no
   floor. `GraphicEqSetBandFreq` accepts frequencies down to 10 Hz (`GraphicEqSet.cpp:555-556`),
   giving `maxQ = -3.75`, hence a negative `bandwidth = w0/Q`, hence `tan(2π·bw)` negative, hence
   an `asin` of a negative `d` — which happens to stay in range, producing a *mirror-image*
   filter. No band table in the tree starts below 20 Hz, but preset band frequencies do reach the
   DSP (`DfxDspEq.cpp:236-240`). **Clamp `q` to at least some small positive value in Rust and note
   the divergence.**

2. **The `1e-30` bias is a permanent DC injection.** It is added to *every* section's output on
   *every* sample (`SosProcess.cpp:576, 617, 623, 897`). With 31 cascaded sections at 48 kHz, the
   accumulated offset is bounded (each section's DC gain multiplies it) but non-zero, and the DC
   blocker that would have removed it is compiled out (§8.4). It is far below the 24-bit noise
   floor, so it is audibly irrelevant — but it means your "silence in → silence out" unit test
   will fail. Assert `|y| < 1e-25` instead of `y == 0.0`.

3. **Off sections do not advance their state.** When a band goes from 0 dB to +6 dB, its `s1`/`s2`
   still hold whatever they held when it was last on (possibly seconds ago), and that stale energy
   is injected into the signal. There is no crossfade, no state reset, no coefficient ramp
   anywhere — `sosSetSection` stores `a1_old`/`a2_old` (`SosSet.cpp:56-57`) as if it intended to,
   but nothing reads them. Dragging an EQ slider **clicks**. If you fix this in Rust (ramp
   coefficients over ~10 ms, or reset state when re-enabling) say so in the changelog, because it
   will not be bit-identical.

4. **`GraphicEqSetNumBands` does not resize the SOS.** It sets `num_bands`
   (`GraphicEqSet.cpp:151`) and calls `GraphicEqReSetAllBandFreqs` + `GraphicEq_InitSections`, but
   never calls `sosNew` or `sosSetNumActiveSections`. The SOS was allocated with whatever
   `DFXP_GRAPHIC_EQ_NUM_BANDS` was at `GraphicEqNew` time — `31`, from `DfxDspEq.cpp:32` via
   `dfxp_EqInit` → `dfxpEqSetEqType(hp_dfxp, DFXP_GRAPHIC_EQ_NUM_BANDS)` (`dfxpEq.cpp:51`).
   Going 31 → 10 leaves sections 10..30 still **active and still filtering** with their old
   coefficients, while `GraphicEqSetBandBoostCut` refuses to touch them
   (`band_num > num_bands` → `NOT_OKAY`, `GraphicEqSet.cpp:272`). This is a real, audible bug. In
   Rust: resize the cascade and zero the tail.

5. **`GraphicEqSetFilterQ` resets the band frequencies and the sample rate.** See §4. Moving the Q
   slider silently reverts to 20 Hz / 20 kHz / 44100 Hz until the next audio buffer corrects the
   rate, discarding any preset band frequencies on the way.

6. **`GraphicEqSetNumBands`'s `switch (num_bands)` default table is dead code.** It only runs when
   `min_freq <= 0 || max_freq <= 0` (`GraphicEqSet.cpp:166`), which is never true after the first
   `GraphicEqReSetAllBandFreqs`, and `GraphicEqReSetAllBandFreqs` overwrites both anyway for the
   five known counts.

7. **`Qntitor.cpp`'s `QNT_RESPONSE_Q_TYPE` can write `q[-1]`.** See §12.2. The original author
   documented the hazard and did not fix it.

8. **`r_boost_cut_original[35]` / `[35]` are 1-based stack arrays** with no bound on the preset's
   claimed band count (`DfxDspEq.cpp:178-186`). A hostile or corrupt preset overflows the stack.

9. **Setting a band's boost to 0 dB and setting it to a frequency above Nyquist are
   indistinguishable afterwards** — both store `0.0` in `sos_center_freq_response`
   (`GraphicEqSet.cpp:289-295`). The user's intent is only recoverable from the registry copy.

---

## Open questions / risks for the Rust port

**Precision policy — the single biggest decision.**
The original designs every coefficient in `f32` (§1), and the two Q limiters in §6.2 exist purely
to paper over the resulting catastrophic cancellation at low frequencies. If you design in `f64`
you get a measurably more accurate filter *and* the limiters become unnecessary — but the EQ will
no longer sound like FxSound, especially in the 20–100 Hz bands where the limiters silently widen
every band. Recommendation: implement `calc_parametric` in `f32` for bit-parity, keep the limiters,
and gate an `f64` "high precision" path behind a config flag once you have A/B listening data.
Either way, keep the *running* state in `f32` — `f64` states would change the denormal behaviour
and make the `1e-30` bias meaningless.

**Are `tan`/`asin`/`atan2`/`pow` bit-identical between MSVC's CRT and glibc?**
No, not guaranteed. Both are correctly rounded to well under 1 ULP for these functions in practice,
but the truncation to `f32` immediately afterwards means a 1-ULP `f64` difference can flip the
`f32` result. The golden vectors in §13 were generated with glibc; expect last-digit differences
against a Windows build. Decide whether your regression tests compare exactly or with a `1e-6`
relative tolerance. (Recommendation: relative tolerance, and a separate audio-level test that
compares impulse responses.)

**Shelving filters are designed but never reachable from the EQ, and the inner loop cannot run
them.** `filtCalcShelf` produces `a1 != b1`, and `Section::tick` assumes `a1 == b1`. The original
source flags this as a known defect (`SosProcess.cpp:525-526`). If the Linux product wants shelf
bands at the spectrum edges (a common request for graphic EQs), you must use `tick_general` for
those sections — which costs one extra multiply. Confirm with the product owner whether shelves are
wanted before designing the section type.

**Is the `bandwidth >= 0.5 → theta = 0.005` hack ever hit in the shipping configuration?**
With the `Q >= 1.0` floor and the five fixed band tables, `w0/Q >= 0.5` requires `f0/fs >= 0.5`,
which the Nyquist gate already blocks. So probably never — but a preset with a custom band
frequency plus `Q_multiplier` interactions could get there. I could not construct a reachable case
from the tree alone; worth a fuzz test.

**Volume levelling is out of scope here but shares the SOS handle.**
`applyVolumeLeveling` (`SosProcess.cpp:37-493`, invoked at `:725` and `:908`) carries ~35 tuning
constants, three one-pole tone probes, a 6-entry power ring and a 30-second peak window. It runs
*after* the EQ and mutates the same output buffer. It needs its own spec document; do not try to
fold it into the EQ port.

**Buffer-level RMS normalisation is stereo-only and buffer-size dependent.**
`current_rms` is computed over exactly one callback buffer (`SosProcess.cpp:680`) and the smoothing
factors (`0.0005 + gain_diff*0.001` attack, effectively `0.5` release after the
`min(smoothing, 0.5)` on `:710`) are **per buffer, not per second**. PipeWire quantum sizes differ
wildly from WASAPI's, so the normaliser's time constants will change behaviour on Linux unless you
rescale them by `buffer_frames / sample_rate`. This needs a deliberate retuning decision and
listening tests.

**The `smoothing_factor = 1` on `SosProcess.cpp:706` looks like a debug leftover**, immediately
neutered by `min(smoothing, 0.5)` on `:710`. The commented-out original was
`0.005f + (gain_diff * 0.01f)`. Which behaviour is "correct" is a product question, not a porting
question.

**Coefficient handoff to the audio thread is entirely unsynchronised in the original.**
`GraphicEqSetBandBoostCut` writes `b0/b1/b2/a1/a2` field-by-field (`SosSet.cpp:60-64`) from the UI
thread while `sosProcessBuffer` reads them on the audio thread — torn reads are possible and only
survive because the coefficients change slowly and the filter is stable for intermediate mixes. In
Rust this is a data race with no safe expression. Use `arc_swap::ArcSwap<Vec<Section>>` (coefficients
only; keep state on the audio thread) or a triple buffer. Note this changes *when* a change takes
effect (next buffer boundary rather than mid-buffer), which is a behavioural divergence, albeit a
strictly better one.

**No sample-rate bounds are enforced anywhere.** `GraphicEqProcess` accepts any `r_samp_freq` and
redesigns. At 192 kHz the 20 Hz band gets `w0 = 1.04e-4`, `a = tan(π(1.04e-4 - 0.25))` ≈ `-0.99935`,
and `1 - a²` ≈ `1.3e-3` in `f32` — roughly 4× worse cancellation than at 48 kHz, and Rule A caps Q
at 1.0 regardless of rate. Whether the low bands are usable at high rates is untested in the
original; test it and consider a rate-dependent Q floor.

**`GRAPHIC_EQ_DEFAULT_MAX_BOOST_OR_CUT` is 20 dB but the UI only exposes 12 dB** (§5). If the Linux
UI ever exposes the full ±20 dB, the low-boost Q warp (Rule B) no longer applies above 6 dB and the
bands become very narrow at Q 4.33 — 31 bands at ±20 dB with Q 4.33 will sound comb-filtered.
Confirm the intended maximum before widening the slider.

**Band-count interpolation exists in two independent, subtly different copies**
(`DfxDspEq.cpp:189-227` and `GraphicEqSet.cpp:207-245`). They disagree on the `nBands == 1` case
(only the second handles it, `GraphicEqSet.cpp:213-216`) and on what happens when the interpolation
index falls out of range (the first leaves the output uninitialised, §14). Unify them in Rust and
pick the `GraphicEqSet` behaviour.
