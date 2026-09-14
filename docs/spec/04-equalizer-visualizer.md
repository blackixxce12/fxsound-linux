# 04 — Graphic Equalizer UI & Spectrum Visualizer

Reverse-engineering spec for the Rust 1.98.1 / egui-eframe 0.36 / PipeWire port.

All citations are `path:line`, relative to the repo root
`/home/blackixxce/Загрузки/fxsound-app-main`. Every number below was read from the
cited line; nothing is inferred unless explicitly labelled **[derived]** (arithmetic
on cited constants) or **[JUCE semantics]** (behaviour of the framework the code
relies on).

Primary sources read in full:

| File | Lines | Role |
|---|---|---|
| `fxsound/Source/GUI/FxEqualizer.h` | 118 | EQ component + two slider subclasses |
| `fxsound/Source/GUI/FxEqualizer.cpp` | 648 | EQ layout, curve painting, slider behaviour |
| `fxsound/Source/GUI/FxVisualizer.h` | 65 | Visualizer component |
| `fxsound/Source/GUI/FxVisualizer.cpp` | 196 (of the concatenated pair) | Bar history, painting, gradient |

Supporting sources read for the numbers the UI depends on:
`fxsound/Source/GUI/FxController.{h,cpp}`, `fxsound/Source/GUI/FxTheme.{h,cpp}`,
`fxsound/Source/GUI/FxProView.{h,cpp}`, `fxsound/Source/GUI/FxAudioControls.{h,cpp}`,
`dsp/DfxDspEq.cpp`, `dsp/ptutil/DspUtil/GraphicEq/*`, `dsp/ptutil/include/GraphicEq.h`,
`dsp/ptutil/DspUtil/spectrum/*`, `dsp/ptutil/include/spectrum.h`,
`dsp/ptutil/dfxp/dfxpSpectrum.cpp`, `dsp/ptutil/include/dfxpDefs.h`,
`dsp/ptutil/Filt/FiltCalcBiqd.cpp`.

---

## 0. Where these two widgets live

`FxProView` owns both as direct members — **not** via the `FxEqualizer::getInstance()`
singleton that `FxEqualizer.h:32-36` declares (that singleton is dead code in this build).

* `FxProView.h:63` — `FxEqualizer equalizer_;`
* `FxProView.h:65` — `FxVisualizer visualizer_;` (added with `addChildComponent`, i.e.
  initially **invisible**, `FxProView.cpp:32`)

Pro view geometry (`FxProView.h:44-52`, `FxProView.cpp:80-99`):

```
FxProView  WIDTH=1040  HEIGHT=491   (→ 511 after update(), FxProView.cpp:69)

 x=20                                                              x=1020
  ┌──────────────────────────────────────────────────────────────────┐  y=16
  │ PanelBackground @ alpha 0.20, rounded r=8, 1000 x (347+offset)    │
  │                                                                  │
  │  ┌ preset_list_ 470x40 @(40,32) ┐   ┌ endpoint_list_ @(530,32) ┐ │
  │                                                                  │
  │  ┌──────────────── FxVisualizer 960 x 120 @ (40, 92) ───────────┐ │
  │  └──────────────────────────────────────────────────────────────┘ │
  │  ┌ FxAudioControls ┐  ┌────────── FxEqualizer 776 x 257 ────────┐ │
  │  │  168 x 257      │  │            @ (224, 228)                 │ │
  │  │  @ (40, 228)    │  │                                          │ │
  │  └─────────────────┘  └──────────────────────────────────────────┘ │
  └──────────────────────────────────────────────────────────────────┘
```

**[derived]** `preset_list_` bottom = 32+40 = 72 → visualizer y = 72+20 = **92**
(`FxProView.cpp:91`). `visualizer_offset` = 120+20 = **140** (`FxProView.cpp:92`);
`AUDIO_Y`=88 → controls/EQ y = 88+140 = **228** (`FxProView.cpp:97-98`);
`audio_controls_` right = 40+168 = 208 → EQ x = 208+16 = **224** (`FxProView.cpp:98`,
width from `FxAudioControls.h:147`). EQ right edge = 224+776 = 1000 = panel right edge.

Both components are set `setOpaque(false)` (`FxEqualizer` inherits the default; the
visualizer sets it explicitly at `FxVisualizer.cpp:104` — line 104 of the concatenated
`.h`+`.cpp` dump, i.e. `FxVisualizer.cpp:18` in the file itself). Both paint their own
rounded-rect background first, so in egui they are simply two opaque rounded panels.

Enablement is driven by the global power state, every repaint
(`FxProView.cpp:117-122`):

```cpp
auto enable_controls = FxModel::getModel().getPowerState();
equalizer_.setEnabled(enable_controls);
visualizer_.setEnabled(enable_controls);
```

---

# PART A — THE GRAPHIC EQUALIZER

## A1. Component constants

All from `fxsound/Source/GUI/FxEqualizer.h:96-105` and `FxTheme.h:44-45`:

| Constant | Value | Source |
|---|---|---|
| `WIDTH` | 776 | FxEqualizer.h:96 |
| `HEIGHT` | 257 | FxEqualizer.h:97 |
| `SLIDER_HEIGHT` | 180 | FxEqualizer.h:98 |
| `LABEL_HEIGHT` | 12 | FxEqualizer.h:99 |
| `SMALL_FONT` | 10 | FxEqualizer.h:100 |
| `ROTARY_SLIDER_HEIGHT` | 36 | FxEqualizer.h:101 |
| `X_MARGIN` | 16 | FxEqualizer.h:102 |
| `Y_MARGIN` | 8 | FxEqualizer.h:103 |
| `MAX_GAIN` | 12.0f | FxEqualizer.h:105 |
| `FxEqSlider::LABEL_HEIGHT` | 12 | FxEqualizer.h:65 |
| `FxTheme::SLIDER_THUMB_RADIUS` | 8 | FxTheme.h:44 |
| `FxTheme::ROTARY_SLIDER_THUMB_RADIUS` | 5 | FxTheme.h:45 |

Controller-side limits: `MIN_GAIN = -12.0f`, `MAX_GAIN = 12.0f`
(`FxController.h:52-53`), `DEFAULT_NUM_EQ_BANDS = 10` (`FxController.h:46`),
`DEFAULT_FILTER_Q = 1.0f` (`FxController.h:50`).

## A2. Band count — values and how it changes

Selectable band counts are exactly **{5, 10, 15, 20, 31}**
(`FxAudioControls.h:106`: `std::vector<int> equalizer_bands_ = { 5, 10, 15, 20, 31 };`).
The combo box items read `"<n> Bands"` (`FxAudioControls.cpp:301`); selection calls
`FxController::setNumEqBands(n)` (`FxAudioControls.cpp:295`), which calls
`dfx_dsp_.setNumBands(n)` and persists `settings_["num_bands"]`
(`FxController.cpp:1778-1782`).

Hard DSP limit: `1..31` (`dsp/ptutil/DspUtil/GraphicEq/GraphicEqSet.cpp:128`), with
`GRAPHIC_EQ_MAX_NUM_BANDS = SOS_MAX_NUM_SOS_SECTIONS`
(`dsp/ptutil/DspUtil/GraphicEq/u_GraphicEq.h:35`). The global
`DFXP_GRAPHIC_EQ_NUM_BANDS` starts at **31** (`dsp/DfxDspEq.cpp:32`) and is rewritten
on every band-count change (`GraphicEqSet.cpp:154`).

### How the UI reacts to a band-count change

There is **no** observer/callback. `FxEqualizer::paint()` polls it every frame:

```cpp
int num_bands = controller.getNumEqBands();
if (num_bands != labels_.size())
    reinit(num_bands);                       // FxEqualizer.cpp:303-308
```

`resized()` does the same guard and bails out with a `repaint()` if the counts disagree
(`FxEqualizer.cpp:241-247`). `reinit(n)` (`FxEqualizer.cpp:67-116`):

1. `removeAllChildren()`
2. clears and resizes `labels_`, `band_boosts_`, `center_frequencies_`, `band_gain_values_`
3. recreates every control (identical to the constructor, `FxEqualizer.cpp:38-60`)
4. `resized(); update(); showValues(true);`

Gains survive a band-count change: `GraphicEqSetNumBands` snapshots the old boost/cut
array and remaps it by *relative position* — equidistant nearest-band selection when
shrinking, linear interpolation when growing (`GraphicEqSet.cpp:139-148, 204-245`).
The same remap logic exists for preset loading at `dsp/DfxDspEq.cpp:189-227`.

**Port note:** in Rust, model the band count as a single source of truth in the app
state and rebuild the band vector on change; you do *not* need the poll-in-paint hack,
but you must keep the gain remap maths byte-for-byte if presets are to be compatible.

## A3. Default centre frequency of every band

Set by `GraphicEqReSetAllBandFreqs()`
(`dsp/ptutil/DspUtil/GraphicEq/GraphicEqSet.cpp:362-528`). The function *overwrites*
`min_band_freq` / `max_band_freq` inside each per-N branch.

| N | min_band_freq | max_band_freq | Source |
|---|---|---|---|
| 5 | 62.5 | 16000 | GraphicEqSet.cpp:432-433 |
| 10 | 62.5 | 16000 | GraphicEqSet.cpp:447-448 |
| 15 | 25 | 16000 | GraphicEqSet.cpp:458-459 |
| 20 | 20 | 16000 | GraphicEqSet.cpp:470-471 |
| 31 | 20 | 20000 | GraphicEqSet.cpp:482-483 |

### N = 5 — `GraphicEqSet.cpp:434`

`{ 62.5, 250.0, 1000.0, 4000.0, 16000.0 }`

### N = 10 — legacy pre-ISO grid — `GraphicEqSet.cpp:449`

`{ 62.5, 115.734, 214.311, 396.85, 734.867, 1360.79, 2519.84, 4666.12, 8640.48, 16000.0 }`

The same literal table is duplicated in the UI for right-click reset
(`FxEqualizer.cpp:617-629`). The comment at `GraphicEqSet.cpp:443-446` states this is
deliberately *not* ISO 266 — factory/community presets were authored against it.

### N = 15 — ISO — `GraphicEqSet.cpp:460-461`

`{ 25, 40, 63, 100, 160, 250, 400, 630, 1000, 1600, 2500, 4000, 6300, 10000, 16000 }`

### N = 20 — ISO — `GraphicEqSet.cpp:472-473`

`{ 20, 31.5, 40, 63, 80, 125, 160, 250, 315, 500, 630, 1000, 1250, 2000, 2500, 4000, 5000, 8000, 10000, 16000 }`

### N = 31 — ISO — `GraphicEqSet.cpp:484-486`

`{ 20, 25, 31.5, 40, 50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600, 2000, 2500, 3150, 4000, 5000, 6300, 8000, 10000, 12500, 16000, 20000 }`

### Any other N (1 < N < 31, not in the table) — `GraphicEqSet.cpp:493-509`

```
f_k = min_band_freq * (max_band_freq / min_band_freq) ^ ((k-1)/(N-1))      k = 1..N
```
`N == 1` is special-cased first: `Q = 1.0`, `f_1 = min_band_freq`
(`GraphicEqSet.cpp:386-392`).

Global clamps applied by `GraphicEqSetBandFreq()`:
`GRAPHIC_EQ_MIN_BAND_FREQ = 10`, `GRAPHIC_EQ_MAX_BAND_FREQ = 21000.0`
(`dsp/ptutil/include/GraphicEq.h:42-43`, applied at `GraphicEqSet.cpp:555-559`).

## A4. Per-band frequency min/max — how neighbours constrain each other

`GraphicEqGetBandFrequencyRange()`
(`dsp/ptutil/DspUtil/GraphicEq/GraphicEqGet.cpp:105-168`). Note the band index here is
1-based; `DfxDspPrivate::getEqBandFrequencyRange` adds 1 (`dsp/DfxDspEq.cpp:289`).

Let `R = max_band_freq / min_band_freq` and `D = 2N - 2`.

```
band 1      : min = min_band_freq                               (GraphicEqGet.cpp:137)
band k > 1  : min = round( min_band_freq * R^(((k-1)*2 - 1)/D) )(GraphicEqGet.cpp:141-145)
              then   min += 1   if min <  1000                  (GraphicEqGet.cpp:146-147)
                     min += 10  if min >= 1000                  (GraphicEqGet.cpp:148-149)

band N      : max = max_band_freq                               (GraphicEqGet.cpp:155)
band k < N  : max = round( min_band_freq * R^((k*2 - 1)/D) )    (GraphicEqGet.cpp:159-163)

N == 1      : min = max = the band's own current frequency; Q forced to 1.0
                                                                (GraphicEqGet.cpp:123-129)
```

**This is the neighbour constraint.** The exponent `(2k-1)/(2N-2)` is the *geometric
midpoint* between the generic log-spaced grid positions of band `k` and band `k+1`.
So band `k`'s upper bound is the geometric mean of the *nominal* grid centres of bands
`k` and `k+1`, and band `k+1`'s lower bound is that same value **+1 Hz** (or **+10 Hz**
above 1 kHz). Bands can therefore never cross or coincide; there is a 1 Hz / 10 Hz
dead-zone between adjacent band ranges. The ends are pinned to the fixed spectrum edges
rather than the (possibly preset-shifted) band frequency — see the comments at
`GraphicEqGet.cpp:136` and `:154`.

**Important mismatch to preserve:** the boundaries are computed from the *generic
log grid*, while the centres for N ∈ {5,10,15,20,31} come from the *hard-coded tables*.
For N=5 and N=15/20/31 the centres therefore do **not** sit at the middle of their own
range (e.g. N=5 band 1: centre 62.5 = the bottom of range 62.5..125).

### Full computed range tables **[derived]** from the formula above

Slider step is `(max - min) / 100` (`FxEqualizer.cpp:55` and `:105`).

#### N = 5

| Band | Centre (Hz) | min (Hz) | max (Hz) | step (Hz) |
|---:|---:|---:|---:|---:|
| 1 | 62.5 | 62.5 | 125 | 0.625 |
| 2 | 250 | 126 | 500 | 3.74 |
| 3 | 1000 | 501 | 2000 | 14.99 |
| 4 | 4000 | 2010 | 8000 | 59.90 |
| 5 | 16000 | 8010 | 16000 | 79.90 |

#### N = 10 (the default)

| Band | Centre (Hz) | min (Hz) | max (Hz) | step (Hz) |
|---:|---:|---:|---:|---:|
| 1 | 62.5 | 62.5 | 85 | 0.225 |
| 2 | 115.734 | 86 | 157 | 0.71 |
| 3 | 214.311 | 158 | 292 | 1.34 |
| 4 | 396.85 | 293 | 540 | 2.47 |
| 5 | 734.867 | 541 | 1000 | 4.59 |
| 6 | 1360.79 | 1010 | 1852 | 8.42 |
| 7 | 2519.84 | 1862 | 3429 | 15.67 |
| 8 | 4666.12 | 3439 | 6350 | 29.11 |
| 9 | 8640.48 | 6360 | 11758 | 53.98 |
| 10 | 16000 | 11768 | 16000 | 42.32 |

#### N = 15

| Band | Centre | min | max | | Band | Centre | min | max |
|---:|---:|---:|---:|---|---:|---:|---:|---:|
| 1 | 25 | 25 | 31 | | 9 | 1000 | 798 | 1264 |
| 2 | 40 | 32 | 50 | | 10 | 1600 | 1274 | 2005 |
| 3 | 63 | 51 | 79 | | 11 | 2500 | 2015 | 3181 |
| 4 | 100 | 80 | 126 | | 12 | 4000 | 3191 | 5047 |
| 5 | 160 | 127 | 199 | | 13 | 6300 | 5057 | 8007 |
| 6 | 250 | 200 | 316 | | 14 | 10000 | 8017 | 12703 |
| 7 | 400 | 317 | 502 | | 15 | 16000 | 12713 | 16000 |
| 8 | 630 | 503 | 797 | | | | | |

#### N = 20

| Band | Centre | min | max | | Band | Centre | min | max |
|---:|---:|---:|---:|---|---:|---:|---:|---:|
| 1 | 20 | 20 | 24 | | 11 | 630 | 567 | 804 |
| 2 | 31.5 | 25 | 34 | | 12 | 1000 | 805 | 1143 |
| 3 | 40 | 35 | 48 | | 13 | 1250 | 1153 | 1625 |
| 4 | 63 | 49 | 69 | | 14 | 2000 | 1635 | 2311 |
| 5 | 80 | 70 | 97 | | 15 | 2500 | 2321 | 3285 |
| 6 | 125 | 98 | 138 | | 16 | 4000 | 3295 | 4670 |
| 7 | 160 | 139 | 197 | | 17 | 5000 | 4680 | 6639 |
| 8 | 250 | 198 | 280 | | 18 | 8000 | 6649 | 9439 |
| 9 | 315 | 281 | 398 | | 19 | 10000 | 9449 | 13419 |
| 10 | 500 | 399 | 566 | | 20 | 16000 | 13429 | 16000 |

#### N = 31

| Band | Centre | min | max | | Band | Centre | min | max |
|---:|---:|---:|---:|---|---:|---:|---:|---:|
| 1 | 20 | 20 | 22 | | 17 | 800 | 711 | 893 |
| 2 | 25 | 23 | 28 | | 18 | 1000 | 894 | 1125 |
| 3 | 31.5 | 29 | 36 | | 19 | 1250 | 1135 | 1416 |
| 4 | 40 | 37 | 45 | | 20 | 1600 | 1426 | 1783 |
| 5 | 50 | 46 | 56 | | 21 | 2000 | 1793 | 2244 |
| 6 | 63 | 57 | 71 | | 22 | 2500 | 2254 | 2825 |
| 7 | 80 | 72 | 89 | | 23 | 3150 | 2835 | 3557 |
| 8 | 100 | 90 | 112 | | 24 | 4000 | 3567 | 4477 |
| 9 | 125 | 113 | 142 | | 25 | 5000 | 4487 | 5637 |
| 10 | 160 | 143 | 178 | | 26 | 6300 | 5647 | 7096 |
| 11 | 200 | 179 | 224 | | 27 | 8000 | 7106 | 8934 |
| 12 | 250 | 225 | 283 | | 28 | 10000 | 8944 | 11247 |
| 13 | 315 | 284 | 356 | | 29 | 12500 | 11257 | 14159 |
| 14 | 400 | 357 | 448 | | 30 | 16000 | 14169 | 17825 |
| 15 | 500 | 449 | 564 | | 31 | 20000 | 17835 | 20000 |
| 16 | 630 | 565 | 710 | | | | | |

Writes are rejected outside the range — `FxController::setEqBandFrequency` returns early
if `freq < min_freq || freq > max_freq` (`FxController.cpp:1853-1858`).

## A5. Q / filter-width control

Two separate things share the name "Q".

**(a) The derived per-EQ Q** — `GraphicEqSet.cpp:515-524`:

```
r = (max_band_freq / min_band_freq) ^ (1 / (N - 1))
Q = sqrt(r) / (r - 1)
Q *= Q_multiplier
if (Q < 1.0) Q = 1.0                      // hard floor
```

**[derived]** values with `Q_multiplier = 1`:

| N | R | r | raw Q | effective Q |
|---:|---:|---:|---:|---:|
| 5 | 256 | 4.000000000 | 0.66666667 | **1.0** (floored) |
| 10 | 256 | 1.851749425 | 1.59764123 | 1.59764123 |
| 15 | 640 | 1.586504933 | 2.14757848 | 2.14757848 |
| 20 | 800 | 1.421654977 | 2.82774260 | 2.82774260 |
| 31 | 1000 | 1.258925412 | 4.33336553 | 4.33336553 |

These match the design comments verbatim (`GraphicEqSet.cpp:410`, `:415`, `:421`).

**(b) The user-facing "Filter Q" slider** — lives in `FxAudioControls`, not in
`FxEqualizer`:

* style `LinearHorizontal`, **range 1 .. 3 step 0.5** (`FxAudioControls.cpp:347-348`)
* format string `"%.1fx"`, default 1.0f (`FxAudioControls.cpp:271`)
* `FxController::setFilterQ` rounds to the nearest 0.5 before storing
  (`FxController.cpp:1825-1830`: `std::round(q * 2.0f) / 2.0f`)
* writing it calls `GraphicEqSetFilterQ` → `GraphicEq_InitSections` → full band/Q
  recompute (`GraphicEqSet.cpp:106-116`)
* `GraphicEqNew` initialises `Q_multiplier = 1` (`GraphicEqInit.cpp:49`)
* "Restore defaults" resets it to `DEFAULT_FILTER_Q = 1.0f`
  (`FxAudioControls.cpp:536`, `FxController.h:50`)

**(c) Additional Q clamping inside the biquad designer** —
`dsp/ptutil/Filt/FiltCalcBiqd.cpp:146-176`:

* Below 60 Hz the Q tapers linearly to 1.0 at 20 Hz
  (`FILT_Q_UPPER_LIMIT_FREQ 60.0`, `FILT_Q_LOWER_LIMIT_FREQ 20.0`,
  `FILT_Q_UPPER_LIMIT 20.0`, `FILT_Q_LOWER_LIMIT 1.0`).
* For `|boost| < 6 dB` the Q is capped at `|boost| * (20 - 0.2)/6 + 0.2`
  (`FILT_BOOST_WARP_LEVEL 6.0`, `FILT_BOOST_MAX_Q 20.0`, `FILT_BOOST_MIN_Q 0.2`).
* `boost == 0.0` turns the section off entirely (`FiltCalcBiqd.cpp:132-137`, and
  `GraphicEqSet.cpp:289-295` also bypasses when `f_c * 2 >= sampling_freq`).

There is **no Q control in the EQ panel itself** — the panel has exactly two controls
per band (gain slider + frequency wheel).

## A6. Boost / cut range

* UI slider: `setRange(-MAX_GAIN, MAX_GAIN, 1.0)` → **−12.0 … +12.0 dB, 1 dB steps**
  (25 discrete positions) — `FxEqualizer.cpp:48` and `:98`.
* Controller guard: rejects `< MIN_GAIN (-12)` or `> MAX_GAIN (+12)`
  (`FxController.cpp:1900-1903`, constants `FxController.h:52-53`).
* `update()` only pushes values in `[-12, +12]` into the slider
  (`FxEqualizer.cpp:219-222`).
* DSP hard clamp is wider: `GRAPHIC_EQ_DEFAULT_MAX_BOOST_OR_CUT = 20.0`
  (`dsp/ptutil/include/GraphicEq.h:46`, applied `GraphicEqSet.cpp:298-302`). Presets
  may therefore contain ±20 dB, which the UI would clamp on display.

## A7. Exact layout geometry

`FxEqualizer::resized()` — `FxEqualizer.cpp:237-281`.

```
x      = X_MARGIN                                   = 16          (:251)
width  = (776 - 16*2) / N   [integer division]      = 744 / N     (:252)
rotary_width = min(36, width)                                     (:253)
show_center_frequencies = (N <= 10)                               (:255-259)
```

**[derived]** column width and the slider's x offset inside the column
(`(width - SLIDER_THUMB_RADIUS*4)/2` = `(width-32)/2`, C integer division):

| N | `width` | offset | slider centre x (band i) | notes |
|---:|---:|---:|---|---|
| 5 | 148 | 58 | `16 + 148i + 58 + 16` = `90 + 148i` | |
| 10 | 74 | 21 | `53 + 74i` | 53 … 719 |
| 15 | 49 | 8 | `40 + 49i` | |
| 20 | 37 | 2 | `34 + 37i` | |
| 31 | 24 | **−4** | `28 + 24i` | **sliders overlap by 8 px** |

> **Quirk to reproduce or fix:** the 32 px-wide click column is wider than the 24 px
> band column at N=31, so adjacent hit areas overlap by 8 px. In JUCE the later-added
> child (higher band index) is on top and wins the click **[JUCE semantics]**.
> In egui you must decide the same tie-break explicitly (allocate band *i+1*'s rect
> after band *i* and let it win), or clamp the hit width to `width`.

### Branch 1 — `N <= 10` (frequency wheels visible) — `FxEqualizer.cpp:264-271`

```
label font  = getNormalFont().withHeight(12)                       (:267)
gain slider = (x + offset, 8, 32, 180)        bottom = 188         (:268)
freq label  = (x, 194, width, 12)             bottom = 206         (:269)
freq wheel  = (x + (width-rw)/2, 210, rw, rw) bottom = 246         (:270)
```

### Branch 2 — `N > 10` (wheels hidden) — `FxEqualizer.cpp:273-277`

```
center_frequencies_[i]->setVisible(false)                          (:264)
label font  = getNormalFont().withHeight(10)                       (:274)
gain slider = (x + offset, 8, 32, 180+36 = 216)  bottom = 224      (:275)
freq label  = (x, 230, width, 24)                bottom = 254      (:276)
```

### The gain slider's usable track (this is the hard part)

`FxTheme::getSliderLayout` overrides JUCE's layout for `LinearVertical`
(`FxTheme.cpp:332-347` → in-file lines `:337-341`):

```cpp
auto y      = layout.sliderBounds.getY()      + (SLIDER_THUMB_RADIUS*2);   // +16
auto height = layout.sliderBounds.getHeight() - (SLIDER_THUMB_RADIUS*2);   // -16
```

JUCE's base layout already reduces a vertical slider by `getSliderThumbRadius()` = 8 on
each end **[JUCE semantics, `LookAndFeel_V2::getSliderLayout`]**, and
`FxTheme::getSliderThumbRadius` returns 8 for non-rotary sliders (`FxTheme.cpp:320-331`).

**[derived]** in slider-local coordinates:

```
region_start = 8 + 16              = 24
region_size  = slider_height - 16 - 16  = slider_height - 32
```

| Branch | slider height | region_start | region_size | px per dB |
|---|---:|---:|---:|---:|
| `N<=10` | 180 | 24 | **148** | 6.1667 |
| `N>10` | 216 | 24 | **184** | 7.6667 |

`Slider::getPositionOfValue(v)` for a vertical slider **[JUCE semantics]**:

```
y_local(v) = region_start + (1 - (v + 12)/24) * region_size
           = 24 + (12 - v)/24 * region_size
```

The EQ curve adds `Y_MARGIN` to convert to panel space (`FxEqualizer.cpp:352-353`,
`:375`) — which is exactly the slider's own `y`, so:

```
y_panel(v) = 32 + (12 - v)/24 * region_size
```

| v (dB) | `N<=10` y_panel | `N>10` y_panel |
|---:|---:|---:|
| +12 | 32 | 32 |
| +6 | 69 | 78 |
| 0 | **106** | **124** |
| −6 | 143 | 170 |
| −12 | 180 | 216 |

**[derived]** the fill baseline used by the curve is
`slider.getBottom() - SLIDER_THUMB_RADIUS` = 188−8 = **180** (`N<=10`) or 224−8 = **216**
(`N>10`) — i.e. exactly the −12 dB line. This is not a coincidence and must be
preserved: the filled area collapses to zero when every band is at −12 dB.

### ASCII map, N = 10

```
FxEqualizer 776 x 257, origin top-left, ControlBackground rounded r=8
 x:0        16                                                        760   776
 y:0  ┌──────────────────────────────────────────────────────────────────┐
      │                                                                  │
  8   │   ┌──┐  ┌──┐  ┌──┐  ...  gain sliders 32 x 180, y 8..188         │
      │   │  │  │  │                                                     │
 32   │ ─ ┼──┼──┼──┼──── +12 dB line (curve top clip)                    │
      │   │▓▓│  │  │   ▓ = dashed track, gradient SliderTrack→VerticalSliderLow
106   │ ─ ┼──┼──┼──┼──── 0 dB                                            │
      │   │  │  │  │                                                     │
180   │ ─ ┴──┴──┴──┴──── −12 dB == curve fill baseline                   │
188   │   └──┘  └──┘  (slider component bottom)                          │
194   │   "62 Hz"  "116 Hz"  ...  freq labels 74 x 12, centredTop        │
210   │    (o)     (o)     ...  rotary wheels 36 x 36                    │
246   │                                                                  │
257   └──────────────────────────────────────────────────────────────────┘
      column pitch 74;  band i centre x = 53 + 74i
```

## A8. Colour table

`FxTheme::theme_colors_` — `fxsound/Source/GUI/FxTheme.cpp:22-29`; enum order
`FxTheme.h:29-31`. Values are `0xRRGGBB` with an implicit alpha byte of `0x00`, so every
call site must apply `.withAlpha(...)`.

| `FxColor` | idx | Dark (mode 0) | Light (mode 1) | Used by |
|---|---:|---|---|---|
| `ControlBackground` | 15 | `#0f0f0f` | `#e0e0e0` | EQ + visualizer panel fill |
| `SliderTrack` | 16 | `#e33250` | `#0a4d66` | curve line, dashed track, rotary arcs |
| `SliderHighlight` | 17 | `#f7546f` | `#53ccff` | focus/drag glow |
| `GraphHigh` | 18 | `#d51535` | `#1ac1ff` | visualizer gradient ends |
| `GraphLow` | 19 | `#fe566a` | `#72d8ff` | visualizer gradient middle |
| `EqStart` | 20 | `#ef4b65` | `#33c8ff` | EQ fill gradient top |
| `EqEnd` | 21 | `#742834` | `#063244` | EQ fill gradient bottom |
| `VerticalSliderLow` | 22 | `#f3f3f3` | `#1c1c1c` | dashed track bottom colour |
| `DefaultText` | 4 | `#b1b1b1` | `#4e4e4e` | labels |

**Disabled state** uses `juce::Colour::withSaturation(0.0f)`, which in JUCE converts
via HSB and yields `grey = max(r,g,b)` on all three channels **[JUCE semantics]**.
**[derived]** precomputed:

| Colour | Dark → grey | Light → grey |
|---|---|---|
| `SliderTrack` | `#e3e3e3` | `#666666` |
| `GraphHigh` | `#d5d5d5` | `#ffffff` |
| `GraphLow` | `#fefefe` | `#ffffff` |
| `EqStart` | `#efefef` | `#ffffff` |
| `EqEnd` | `#747474` | `#444444` |
| `SliderHighlight` | `#f7f7f7` | `#ffffff` |

Rotary arc colours are registered once in `FxTheme::init`
(`FxTheme.cpp:91-92`):

* `rotarySliderOutlineColourId` = `SliderTrack` @ **alpha 0.2**
* `rotarySliderFillColourId` = `SliderTrack` @ **alpha 1.0**

Fonts: `getNormalFont()` = Gilroy SemiBold @ 17 px base (`FxTheme.cpp:466-469`,
loaded at `FxTheme.cpp:386-388`), re-heighted to 12 or 10 for EQ labels.

## A9. The response curve — exact drawing algorithm

`FxEqualizer::paint()` — `FxEqualizer.cpp:283-394`. Drawn by the **parent**, so it sits
*behind* all slider children **[JUCE semantics]**.

```
 1. if (controller.getNumEqBands() != labels_.size()) reinit(n);          (:303-308)

 2. fill background:
      colour  = ControlBackground @ alpha 1.0                            (:310)
      shape   = getLocalBounds() rounded rect, corner radius 8.0f        (:311)

 3. colours:
      line_colour        = SliderTrack @ 1.00                            (:315)
      gradient_colour_1  = EqStart     @ 0.34                            (:316)
      gradient_colour_2  = EqEnd       @ 0.00      (fully transparent)   (:317)

 4. if (!isEnabled()) desaturate all three                               (:319-324)
    else refresh tooltips on every band                                  (:326-343)

 5. if (highlight_mode_) desaturate gradient_colour_2 only               (:345-348)

 6. POLYLINE — one stroked segment per adjacent pair:                    (:350-370)
      for i in 0 .. N-2:
          y0 = band_boosts_[i]  ->getPositionOfValue(value_i)   + 8
          y1 = band_boosts_[i+1]->getPositionOfValue(value_i+1) + 8
          x0 = band_boosts_[i]  ->getX() + 16      (width/2 = 32/2)
          x1 = band_boosts_[i+1]->getX() + 16
          path.addLineSegment(Line(x0,y0,x1,y1), 1.0);
          colour = (slider_i enabled || slider_i+1 enabled)
                     ? line_colour : line_colour.withSaturation(0);
          g.strokePath(path, PathStrokeType(1.0));
          path.clear();

 7. FILL POLYGON — one closed subpath over all bands:                    (:372-389)
      for i in 0 .. N-1:
          x = band_boosts_[i]->getX() + 16
          y = band_boosts_[i]->getPositionOfValue(value_i) + 8
          if i == 0    : path.startNewSubPath(x, bottom_i)   // bottom = getBottom()-8
          path.lineTo(x, y)
          if i == N-1  : path.lineTo(x, bottom_i)
      path.closeSubPath();

 8. gradient = ColourGradient(gradient_colour_1, 0, band_boosts_[1]->getY(),
                              gradient_colour_2, 0, band_boosts_[1]->getBottom(),
                              /*isRadial=*/false);                        (:391)
    g.setFillType(gradient); g.fillPath(path);                            (:392-393)
```

### Notes an implementer must not miss

* **Line width is effectively ~2 px, not 1.** `Path::addLineSegment(line, 1.0)` builds
  the *outline of a 1 px-thick quad*, and `strokePath(..., PathStrokeType(1.0))` then
  strokes that outline with a 1 px pen **[JUCE semantics]**. Net visual weight ≈ 2 px
  with a hollow 0-width core. In egui, `Stroke::new(1.5, colour)` on a
  `Shape::line_segment` is the closest practical match; `2.0` is also defensible.
* **Segments are drawn independently** (path cleared each iteration), so there are no
  joins — butt caps at every vertex. Visible as tiny notches at steep angles.
* **The fill gradient is anchored to band 1**, not to the panel:
  `y_top = band_boosts_[1]->getY() = 8`, `y_bottom = band_boosts_[1]->getBottom()` =
  **188** (`N<=10`) or **224** (`N>10`). Colour stops: `EqStart@0.34` at y=8 →
  `EqEnd@0.0` at y=188/224, linear. Because `EqEnd` has alpha 0, the fill fades to
  fully transparent at the bottom.
* **`band_boosts_[1]` is indexed unconditionally** (`:391`) — the code would UB with
  N < 2. Bands are always ≥ 5 in practice, but a Rust port should guard.
* `for (auto i = 0; i < band_boosts_.size() - 1; i++)` at `:350` compares `int` to
  `size_t`; with `size() == 0` this underflows. Same guard applies.
* The curve is **piecewise linear between band positions** — it is *not* a computed
  filter transfer function. If you want a true response curve in the Rust port, see
  §A5(c) and `FiltCalcBiqd.cpp:109-222` for the actual biquad; but to look identical,
  draw straight segments.

## A10. Draggable band handles & hit testing

The "handles" are the thumbs of the child `FxEqSlider` components. There is no custom
hit-testing code — JUCE's `Slider` owns the whole component rect.

* **Hit rectangle** = the slider component bounds = `32 x 180` (or `32 x 216`),
  positioned as in §A7. That is the *entire* column strip, not just the thumb.
* **Click-to-jump:** `Slider::setSliderSnapsToMousePosition` defaults to `true`
  **[JUCE semantics]**, and the code never disables it — so a left mouse-down anywhere
  in the strip snaps the value to the y under the cursor, then drags continuously.
* **Value → y mapping** is the inverse of §A7, clamped to `[region_start,
  region_start+region_size]`, then snapped to the 1 dB grid.
* **Thumb rendering** (`FxTheme::drawLinearSlider`, `FxTheme.cpp:181-216`, in-file
  `:186-216`):
  * dashed centre line, dash pattern `{5, 2}`, 1 px wide, from `(x+width/2, y)` to
    `(x+width/2, y+height)` where `x,y,width,height` are the *layout* bounds
    `(0, 24, 32, region_size)` **[derived]**;
  * its gradient runs `SliderTrack@0.4` at component y=0 → `VerticalSliderLow@0.4` at
    component y=`height` (i.e. `region_size`), **not** at the line's endpoints — a
    1-for-1 port must use the same mismatched anchors or accept a slightly different
    ramp;
  * thumb = `Slider_Thumb.svg` (`Slider_Thumb_blue.svg` in light mode,
    `FxTheme.cpp:37`/`:44`) drawn into a `16 x 16` box centred at
    `(x + width/2, sliderPos)`; the greyed `Slider_Thumb_bw.svg` when disabled;
  * while dragging **or** focused: a `SliderHighlight @ 0.1` rounded rect,
    `(x + (width-32)/2, y, 32, height)` expanded by `(0, 8)`, corner radius **20**.
* **Cursor:** `PointingHandCursor` when enabled, `NormalCursor` when disabled
  (`FxEqualizer.cpp:400`, `:428-438`).
* **Keyboard:** `setWantsKeyboardFocus(true)` (`FxEqualizer.cpp:409`); Up/Down change
  the value by `getInterval()` = 1 dB (`FxEqualizer.cpp:461-478`).
* **Mouse wheel** works via JUCE's default `Slider` wheel handling **[JUCE semantics]**
  (never disabled in this code).
* **Right mouse button → reset that band to 0 dB** with notification
  (`FxEqualizer.cpp:481-494`).

`FxEqSlider::valueChanged()` (`FxEqualizer.cpp:446-459`) pushes to the controller only
when the value actually differs from `getEqBandBoostCut(band)`, then re-renders the
floating gain label.

## A11. The floating gain label

* Child `Label` of the slider, `setInterceptsMouseClicks(false,false)`
  (`FxEqualizer.cpp:404-407`), font `getNormalFont().withHeight(12)`,
  `Justification::centred`.
* Bounds: full slider width × 12 px (`FxEqualizer.cpp:440-444`), y set to
  `getPositionOfValue(value) - SLIDER_THUMB_RADIUS*3` = **pos − 24** in slider-local
  space (`FxEqualizer.cpp:419-420` and `:456-457`) → its bottom edge sits 12 px above
  the thumb centre **[derived]**.
* Text format (`FxEqualizer.cpp:416` and `:453`):
  * `value == 0.0` → `"%.0f"` → `"0"`
  * otherwise → `"%+.0f"` → `"+3"`, `"-7"`
* Visibility: `showValue(show)` sets `visible = show && isEnabled()`
  (`FxEqualizer.cpp:423-426`). `FxProView::update()` calls `showValues(true)`
  unconditionally — the comment at `FxProView.cpp:70` says values are always visible
  since v2.0 "otherwise they do not appear on touch screens".

## A12. Frequency labels — exact formatting

`FxEqualizer::FxBandCenterFreqSlider::setFrequency` — `FxEqualizer.cpp:505-544`.
Guarded by `if (value > 0)` (`:510`). The band count switches the whole format:

| Band count | Condition | Format string | Example |
|---|---|---|---|
| `N >= 15` | `value >= 10000` | `"%.0f\nkHz"` | `16\nkHz` |
| `N >= 15` | `value >= 1000` | `"%.1f\nkHz"` | `1.0\nkHz`, `6.3\nkHz` |
| `N >= 15` | else | `"%.0f\nHz"` | `250\nHz` |
| `N < 15` | `value > 1000` | `"%.2f kHz"` | `1.36 kHz`, `16.00 kHz` |
| `N < 15` | else | `"%.0f Hz"` | `62 Hz`, `1000 Hz` |

Note the deliberate asymmetry: `>= 1000` for N≥15 vs **`> 1000`** for N<15, so exactly
1000 Hz renders as `"1000 Hz"` in the 10-band view but `"1.0\nkHz"` in the 15+ view.
The `\n` is real: for N>10 the label box is `LABEL_HEIGHT*2` = 24 px tall
(`FxEqualizer.cpp:276`) and the font drops to 10 px (`:274`).

`%.0f` truncates-to-nearest: 62.5 → `"62"` (banker's/round-half-even in glibc; MSVC
rounds half away from zero — **a 1-digit divergence risk**, see Open questions).

Labels are `Justification::centredTop` (`FxEqualizer.cpp:44`, `:94`) and span the full
column width.

## A13. The frequency wheel (rotary slider)

`FxBandCenterFreqSlider` — constructed as `Slider(Rotary, NoTextBox)`
(`FxEqualizer.cpp:496`).

* **Rotary parameters:** `setRotaryParameters(3.66519f, 8.90118f, true)`
  (`FxEqualizer.cpp:53`, `:103`). **[derived]** 3.66519 rad = **210°**,
  8.90118 rad = **510°** (= 150° + 360°); JUCE measures clockwise from 12 o'clock, so
  the arc starts at the 7-o'clock position and sweeps **300° clockwise**
  (5.23599 rad) to the 5-o'clock position. `stopAtEnd = true`.
* **Range:** `setRange(min_freq, max_freq, (max_freq-min_freq)/100)`
  (`FxEqualizer.cpp:55`, `:105`) — 100 steps across the band's range (see §A4 tables).
* **Rendering** (`FxTheme::drawRotarySlider`, `FxTheme.cpp:257-318`, in-file
  `:259-318`): bounds reduced by 2 → **32×32** inside the 36×36 box;
  `radius = 16`; `lineW = 5.0f`; `arcRadius = 16 − 2.5 = 13.5` **[derived]**.
  Background arc (full 300° sweep) stroked in `SliderTrack@0.2` with curved/rounded
  caps; value arc (start → current) in `SliderTrack@1.0`; thumb = the same SVG in a
  `10 x 10` box (radius 5, `FxTheme.cpp:320-331`) at
  `(cx + 13.5·cos(θ − π/2), cy + 13.5·sin(θ − π/2))`.
  When focused, a `DropShadow` of `SliderHighlight@0.1` is drawn for the background arc.
* **Hidden entirely when `N > 10`** (`FxEqualizer.cpp:264`).
* **Inert when `N >= 15`:** `mouseDown` returns before calling
  `Slider::mouseDown` (`FxEqualizer.cpp:592-593`), so even drag does nothing.
* **Keyboard:** Up/Down step by `getInterval()` (`FxEqualizer.cpp:570-587`).
* **Write-back:** `valueChanged()` pushes to `setEqBandFrequency` only on change, then
  re-formats the label (`FxEqualizer.cpp:558-568`).

## A14. Reset / flat behaviour

Four distinct reset paths:

1. **Right-click a gain slider → that band to 0 dB.**
   `setValue(0.0, sendNotification)` — `FxEqualizer.cpp:481-494`.

2. **Right-click a frequency wheel → that band to its default centre frequency.**
   `FxEqualizer.cpp:589-647`. Only for `N < 15`:
   * `N == 5`: literal table `{62.5, 250, 1000, 4000, 16000}` (`:600-607`)
   * `N == 10`: literal legacy table (`:617-629`, identical to §A3)
   * any other `N < 15`: **`f = (int)(20 * pow(20000/20, band/(N-1)))`** (`:637-639`)
     — note this hard-codes 20 Hz / 20 kHz regardless of the band's actual
     `min_band_freq`/`max_band_freq`, and truncates to `int`. For N ∉ {5,10} this can
     land outside the band's allowed range, in which case
     `FxController::setEqBandFrequency` silently rejects it
     (`FxController.cpp:1855-1858`). **Treat as a bug; see Open questions.**

3. **DSP-level flat:** `DfxDspPrivate::resetEQ()` zeroes bands `1..DFXP_GRAPHIC_EQ_NUM_BANDS`
   (`dsp/DfxDspEq.cpp:109-122`). **Not reachable from the EQ UI in this build.**

4. **"Restore defaults" button** (in `FxAudioControls`, not the EQ panel) —
   `FxEqualizerControl::restoreDefaults()`, `FxAudioControls.cpp:529-544`:
   `num_bands = 10`, `volume_leveling = 0.0`, `balance = 0.0`, `filter_q = 1.0`,
   `master_gain = 0.0` (`FxController.h:46-51`). It does **not** flatten band gains —
   the band-count change remaps them (§A2).

Fresh-EQ defaults: every `band_gain_values_[i] = 0` at construction
(`FxEqualizer.cpp:59`, `:109`), and a preset with no EQ block is loaded as flat with
EQ on (`dsp/DfxDspEq.cpp:144-157`).

## A15. The EQ on/off toggle

There is **no dedicated EQ on/off control in the GUI of this build.**

* The DSP has one: `DfxDspPrivate::eqOn(bool)` → `eqSetProcessingOn(STORAGE_TYPE_ALL, …)`
  which writes `eq_processing_on_` and a `…\LastUsed\EQ\EQOn` registry value
  (`dsp/DfxDspEq.cpp:136-148`, `:53-86`); reading back at `:91-134` defaults to **on**
  (`*ip_on = IS_TRUE`, `:99`). Exposed as `DfxDsp::eqOn` (`dsp/include/DfxDsp.h:48`).
  **Nothing in `fxsound/Source/` ever calls it** (verified by grep).
* What the user actually toggles is the global power button →
  `FxController::setPowerState` → `powerOn()` → `dfx_dsp_.powerOn()`
  (`FxController.cpp:1007-1029`, `:1727-1749`).
* That propagates to the widgets as `setEnabled()` in `FxProView::paint`
  (`FxProView.cpp:117-122`), which is what produces the desaturated EQ curve and
  greyed thumbs.
* Turning off a band without removing it = set its gain to 0 dB, which turns the
  biquad section off (`GraphicEqSet.cpp:289-295`).

**Port recommendation:** expose a real "EQ bypass" toggle in the Rust UI (it is one
bool on the filter graph), but keep the enable/disable visual states exactly as
specified above, driven by the global power state.

## A16. Alt-drag "solo / highlight" mode

An undocumented power feature (`FxEqualizer.cpp:123-210`). `FxEqualizer` is a
`Slider::Listener` and a `Timer`.

**On drag start** (`sliderDragStarted`, `:123-149`), if
`ModifierKeys::getCurrentModifiersRealtime().isAltDown()` and not already in the mode:

```
highlight_mode_ = true
startTimerHz(30)                                  // 30 Hz, :136
for every OTHER band:
    setEnabled(false); showValue(false)
    band_gain_values_[i] = controller.getEqBandBoostCut(i)   // snapshot
```

**Timer tick** (`timerCallback`, `:172-210`), for every *disabled* band, walks its gain
**1 dB per tick** toward the target `-(MAX_GAIN - 2)` = **−10 dB**:

```
gain = controller.getEqBandBoostCut(i)
if (gain != -10) {
    done = true
    if (gain < -10) set gain + 1      // walk up from −12
    else            set gain - 1      // walk down from ≥ −9
}
if (!done) stopTimer()
```

**[derived]** worst case 0 dB → −10 dB takes 10 ticks = **333 ms** at 30 Hz.

**On drag end** (`sliderDragEnded`, `:151-170`): re-enable, re-show values, and restore
each snapshotted gain with `dontSendNotification`.

Side effect on painting: while `highlight_mode_` is true, `gradient_colour_2`
(the EQ fill's bottom stop) is desaturated (`:345-348`), and each polyline segment is
desaturated unless at least one of its two endpoints is enabled (`:360-367`) — so the
soloed band's two segments stay coloured and everything else goes grey.

## A17. Tooltips

`FxEqualizer::paint` re-assigns tooltips each frame while enabled
(`FxEqualizer.cpp:326-343`), gated on `!controller.isHelpTooltipsHidden()`
(`FxController.cpp:2302`, user setting in `FxSettingsDialog.cpp:396`):

* Per-band gain tooltips — a 10-entry `StringArray` at `FxEqualizer.cpp:285-294`,
  applied **only when `num_bands == 10`** (`:334`). Band 1 =
  *"Hyper-low Bass - First band for very low frequencies down to 20 Hz."*, …,
  band 10 = *"The highest range of average human hearing. …"* All wrapped in `TRANS()`
  for localisation.
* One shared 4-line tooltip for every frequency wheel (`:296-299`, applied `:335`).
* When hidden, both are set to `""` (`:339-340`).
* Tooltip chrome: `FxTheme::getTooltipBounds` pads +20 w / +12 h and offsets the popup
  by ±18/36 px horizontally and ±12 px vertically (`FxTheme.cpp:515-526`).

## A18. Rust / egui render loop for the EQ

```rust
// ---- model -------------------------------------------------------------
const MAX_GAIN: f32 = 12.0;
const X_MARGIN: f32 = 16.0;
const Y_MARGIN: f32 = 8.0;
const THUMB_R:  f32 = 8.0;
const PANEL: Vec2 = vec2(776.0, 257.0);

struct Band { gain_db: f32, freq_hz: f32, f_min: f32, f_max: f32, enabled: bool }

struct EqLayout { n: usize, col_w: f32, slider_h: f32, region_size: f32, wheels: bool }

impl EqLayout {
    fn new(n: usize) -> Self {
        let col_w = ((776 - 32) / n as i32) as f32;          // INTEGER division
        let wheels = n <= 10;
        let slider_h = if wheels { 180.0 } else { 216.0 };    // 180 + 36
        Self { n, col_w, slider_h, region_size: slider_h - 32.0, wheels }
    }
    fn col_x(&self, i: usize) -> f32 { X_MARGIN + self.col_w * i as f32 }
    fn slider_x(&self, i: usize) -> f32 {
        // C integer division, truncating toward zero — negative at n = 31
        self.col_x(i) + (((self.col_w as i32 - 32) / 2) as f32)
    }
    fn centre_x(&self, i: usize) -> f32 { self.slider_x(i) + 16.0 }
    fn y_of(&self, gain_db: f32) -> f32 {
        32.0 + (12.0 - gain_db) / 24.0 * self.region_size     // panel space
    }
    fn gain_of(&self, y: f32) -> f32 {
        (12.0 - (y - 32.0) * 24.0 / self.region_size).clamp(-12.0, 12.0).round()
    }
    fn baseline(&self) -> f32 { Y_MARGIN + self.slider_h - THUMB_R }  // 180 or 216
}
```

```rust
// ---- draw --------------------------------------------------------------
fn eq_ui(ui: &mut egui::Ui, st: &mut EqState, theme: &Theme) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(PANEL, egui::Sense::hover());
    let p  = ui.painter_at(rect);
    let o  = rect.min.to_vec2();                       // panel-space → screen
    let lay = EqLayout::new(st.bands.len());
    let on  = st.power_on;
    let sat = |c: Color32| if on { c } else { desaturate(c) };  // grey = max(r,g,b)

    // 1. background
    p.rect_filled(rect, 8.0, theme.control_background);

    // 2. fill polygon, band 1-anchored vertical gradient
    //    EqStart@0.34 at panel y=8  →  EqEnd@0.00 at panel y = Y_MARGIN+slider_h
    let top = Y_MARGIN;
    let bot = Y_MARGIN + lay.slider_h;                 // 188 or 224
    let mut poly: Vec<Pos2> = Vec::with_capacity(lay.n + 2);
    poly.push(pos2(lay.centre_x(0), lay.baseline()));
    for (i, b) in st.bands.iter().enumerate() {
        poly.push(pos2(lay.centre_x(i), lay.y_of(b.gain_db)));
    }
    poly.push(pos2(lay.centre_x(lay.n - 1), lay.baseline()));
    // egui has no gradient brush: emit the polygon as horizontal 1 px strips, or
    // build a Mesh with per-vertex colours. Mesh is cheaper and exact:
    let mesh = gradient_polygon_mesh(
        &poly, o,
        (top, sat(theme.eq_start).linear_multiply(0.34)),
        (bot, sat(theme.eq_end).linear_multiply(0.00)),
    );
    p.add(egui::Shape::mesh(mesh));

    // 3. polyline, per-segment colour, ~1.5 px
    for i in 0..lay.n.saturating_sub(1) {
        let lit = st.bands[i].enabled || st.bands[i + 1].enabled;
        let c = if lit { sat(theme.slider_track) } else { desaturate(theme.slider_track) };
        p.line_segment(
            [pos2(lay.centre_x(i),     lay.y_of(st.bands[i].gain_db))     + o,
             pos2(lay.centre_x(i + 1), lay.y_of(st.bands[i + 1].gain_db)) + o],
            egui::Stroke::new(1.5, c),
        );
    }

    // 4. per-band interactive column, then the visuals on top
    for i in 0..lay.n {
        let col = egui::Rect::from_min_size(
            pos2(lay.slider_x(i), Y_MARGIN) + o, vec2(32.0, lay.slider_h));
        let r = ui.interact(col, ui.id().with(("eqband", i)),
                            egui::Sense::click_and_drag());
        if r.secondary_clicked() { st.set_gain(i, 0.0); }         // right-click → 0 dB
        else if (r.dragged() || r.clicked()) && on {
            if let Some(pos) = r.interact_pointer_pos() {
                st.set_gain(i, lay.gain_of(pos.y - o.y));         // snap-to-mouse
            }
        }
        if r.hovered() { ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand); }
        r.on_hover_text(BAND_TIPS_10[i]);                         // only when n == 10
        draw_dashed_track(&p, &lay, i, o, on, theme);             // 5 on / 2 off, 1 px
        draw_thumb(&p, lay.centre_x(i) + o.x,
                   lay.y_of(st.bands[i].gain_db) + o.y, 8.0, on, theme);
        draw_gain_label(&p, &lay, i, o, st.bands[i].gain_db, theme); // pos − 24, 12 px
        if lay.wheels { freq_wheel(ui, &lay, i, o, st, theme); }     // 210 y, 36 x 36
        draw_freq_label(&p, &lay, i, o, st.bands[i].freq_hz, lay.n, theme);
    }
    resp
}
```

Gradient helper: build an `egui::Mesh` whose vertex colours are `lerp` of the two stops
by `(y - top) / (bot - top)` clamped to `0..=1`, triangulating the polygon as a fan from
the baseline — since the polygon is x-monotone this is trivially correct.

Keyboard: give each column an `egui::Id`, track focus with `ui.memory_mut(|m| m.request_focus(id))`,
and map `ArrowUp`/`ArrowDown` to ±1 dB (gain) and ±`(f_max-f_min)/100` (wheel).

---

# PART B — THE SPECTRUM VISUALIZER

## B1. Constants

`fxsound/Source/GUI/FxVisualizer.h:51-53`:

| Constant | Value |
|---|---|
| `WIDTH` | 960 |
| `HEIGHT` | 120 |
| `NUM_BARS` | 10 |

`FxController::NUM_SPECTRUM_BANDS = 10` (`FxController.h:45`), matching
`DFXP_SPECTRUM_NUM_BANDS = 10` (`dsp/ptutil/include/dfxpDefs.h:153`) and
`SPECTRUM_MAX_NUM_BANDS = 10` (`dsp/ptutil/include/spectrum.h:25`).

**Total rectangles drawn per frame = `NUM_SPECTRUM_BANDS * NUM_BARS` = 100**
(`FxVisualizer.cpp:94`, `:166`, `:211` — line numbers in the concatenated `.h`+`.cpp`
listing; subtract 66 for in-file `.cpp` line numbers).

## B2. Where the values come from

Chain, bottom to top:

```
audio callback
  └─ spectrumProcess()                  dsp/ptutil/DspUtil/spectrum/spectrumProcess.cpp:31
       └─ 10 x spectrum_UpdateFilter()                                           :156
            └─ sFilt[i].level  (0.0 .. 1.0)
  └─ every 40 ms: band_values[i] = band_buf[delay_index + i]                      :145-146
spectrumGetBandValues()                 dsp/ptutil/DspUtil/spectrum/spectrumGet.cpp:30
dfxpSpectrumGetBandValues()             dsp/ptutil/dfxp/dfxpSpectrum.cpp:105
DfxDspPrivate::getSpectrumBandValues()  dsp/DfxDspPrivate.cpp:553
DfxDsp::getSpectrumBandValues()         dsp/DfxDsp.cpp:198
FxController::getSpectrumBandValues()   fxsound/Source/GUI/FxController.cpp:2887
FxVisualizer::update()                  fxsound/Source/GUI/FxVisualizer.cpp:177 (in-file :111)
```

`FxController::getSpectrumBandValues` substitutes **0.01** for every band when
`audio_process_on_` is false (`FxController.cpp:2896-2903`).

## B3. Frequency mapping of the 10 spectrum bands

From the design block in `dsp/ptutil/DspUtil/spectrum/spectrumReset.cpp:101-115`:

**Centre frequencies (Hz), logarithmically spaced, √10-per-half-decade:**

| Band | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
|---|---|---|---|---|---|---|---|---|---|---|
| f_c | 56.23 | 100 | 177.83 | 316.228 | 562.34 | 1000 | 1778.28 | 3162.28 | 5623.4 | 10000 |

**−3 dB crossover points (Hz)** — 11 edges:
`42.17, 74.99, 133.35, 237.14, 421.70, 749.89, 1333.52, 2371.37, 4216.97, 7498.94, 13335.21`

Each band is a **1st-order Butterworth bandpass realised as a 2-pole resonant filter**,
designed at **44100 Hz** with `mkfilter` (design dumps at `spectrumReset.cpp:170-521`).
The coefficients are **hard-coded and never re-designed for other sample rates** —
at 48 kHz all centres shift up by 48000/44100 ≈ 8.8 %. Reproduce as-is for fidelity,
or fix it in the port (see Open questions).

| Band | `a1` | `a2` | design gain (divisor) | warp |
|---:|---|---|---|---:|
| 1 | 1.9952707978 | −0.9953348411 | 4.245657595e+02 | 0.6 |
| 2 | 1.9915173377 | −0.9917194870 | 2.391965397e+02 | 0.6 |
| 3 | 1.9846835136 | −0.9853206989 | 1.349296378e+02 | 1.0 |
| 4 | 1.9720410075 | −0.9740444157 | 7.631087758e+01 | 1.0 |
| 5 | 1.9480305935 | −0.9543009461 | 4.334342216e+01 | 1.3 |
| 6 | 1.9006550741 | −0.9201218454 | 2.479951362e+01 | 1.3 |
| 7 | 1.8025225345 | −0.8620772515 | 1.436694455e+01 | 1.3 |
| 8 | 1.5891186613 | −0.7664106181 | 8.490790030e+00 | 1.3 |
| 9 | 1.1149497494 | −0.6153052550 | 5.169741233e+00 | 1.5 |
| 10 | 0.1311997923 | −0.3874425954 | 3.264452631e+00 | 1.5 |

Coefficients: `spectrumReset.cpp:118-165`. Warps: `SPECTRUM_BAND_1_WARP … _10_WARP`
in `dsp/ptutil/include/spectrum.h:28-37`.

```
gain[i] = (sensitivity * warp[i]) / (num_channels * design_gain[i])       spectrumReset.cpp:120 …
sensitivity = SPECTRUM_DEFAULT_SENSITIVITY (1.0) * SPECTRUM_SENSITIVITY_FACTOR (4.5) = 4.5
              spectrum.h:52, u_spectrum.h:26, spectrumSet.cpp:75 (in-file :59)
num_channels = 2 by default                                              spectrumInit.cpp:58
```

## B4. Smoothing / decay / peak-hold maths — exact constants

There is **no peak-hold anywhere**, and **no smoothing in the GUI**. All temporal
behaviour is the one-pole mean-square smoother in
`spectrum_UpdateFilter` (`spectrumProcess.cpp:156-193`):

```
// shared across all 10 bands, computed once per internal sample:
input_sum = (L + R) - (L + R)[n-2]        // (1 - z^-2) numerator      :68-82
            (0.0 when processing is off)

// per band:
out              = input_sum + a1*y1 + a2*y2 + 1.0e-5   // anti-denormal bias   :162
y2 = y1; y1 = out
out             *= gain                                                        :165
tmp              = out * out                                                   :167
squared_filtered = one_minus_alpha * tmp + alpha * squared_filtered             :171
level            = (squared_filtered > 1.0) ? 1.0 : fast_sqrt(squared_filtered) :173-191
```

**Smoothing coefficient** (`spectrumSet.cpp:47`, in-file `:31`):

```
alpha           = exp( -time_constant / internal_samp_freq )
one_minus_alpha = 1 - alpha
time_constant   = SPECTRUM_DEFAULT_TIME_CONSTANT = 10.0     spectrum.h:47
                  (allowed 1.0 .. 200.0                     spectrum.h:43-44)
```

**[derived]** exponential time constant τ = `internal_samp_freq / time_constant` samples
= **1 / time_constant seconds = 0.100 s**, independent of sample rate.

| fs | alpha | one_minus_alpha |
|---:|---|---|
| 44100 | 0.99977327 | 2.26743e-4 |
| 48000 | 0.99979168 | 2.08329e-4 |

**Internal rate reduction** (`spectrumReset.cpp:52-76`):

| Actual fs | internal fs | `internal_rate_ratio` (loop stride) |
|---|---|---|
| ≤ 48000 | fs | 1 |
| > 48000, < 192000 | fs / 2 | 2 |
| 192000 | fs / 4 | 4 |

Limits: `SPECTRUM_MAXIMUM_SAMP_FREQ 192000.0`,
`SPECTRUM_MAXIMUM_INTERNAL_SAMP_FREQ 48000.0`,
`SPECTRUM_NORMALIZED_SAMP_FREQ 44100.0` (`u_spectrum.h:29-31`).

**Fast square root** (`spectrumProcess.cpp:177-191`): a float bit-hack
(`u.tmp -= 1<<23; u.tmp >>= 1; u.tmp += 1<<29;`) with ≤ 6 % error. In Rust use
`f32::sqrt()` — the 6 % error is not worth reproducing; note it changes bar heights by
up to 6 % if you compare screenshots.

**Output clamp:** `SPECTRUM_MIN_OUTPUT_VALUE 0.0`, `SPECTRUM_MAX_OUTPUT_VALUE 1.0`
(`spectrum.h:39-40`). The GUI re-checks and zeroes anything outside `[0,1]`
(`FxVisualizer.cpp:181-184`, in-file `:115-118`).

**Display latency compensation** (`spectrumProcess.cpp:105-146`,
`spectrumSet.cpp:104` in-file `:88`):

```
delay_count = (int)( host_buffer_delay_secs / refresh_rate_secs + 0.5 )
band_values[i] = band_buf[ delay_index + i ]      // ring buffer, delay_count frames back
SPECTRUM_MAX_DELAY_SECS = 5.0, SPECTRUM_MIN_DELAY_SECS = 0.0      spectrum.h:54-55
```

## B5. Refresh timing

| Layer | Rate | Source |
|---|---|---|
| DSP band-value store | every **40 ms** (25 Hz) | `DFXP_SPECTRUM_REFRESH_RATE_MSECS 40`, `dsp/ptutil/include/dfxpDefs.h:168`; used at `dfxpSpectrum.cpp:57` |
| GUI, JUCE ≥ 8 | vblank-driven, gated to **1/30 s** | `FxVisualizer.cpp:123` (`constexpr double fps_interval = 1.0/30.0`) |
| GUI, JUCE < 8 (running) | `setFramesPerSecond(30)` | `FxVisualizer.cpp:143` |
| GUI, JUCE < 8 (paused) | `setFramesPerSecond(10)` | `FxVisualizer.cpp:160`; also the constructor default `:101` |
| Controller poll for "audio is playing" | 100 ms timer, 5 consecutive ticks to flip | `FxController.cpp:1735` (`startTimer(100)`), `:2076-2099` |

**[derived]** the GUI samples a 25 Hz source at 30 Hz, so roughly one in six frames
repeats the previous band values. The bar history in §B6 advances regardless, so a
repeated value appears twice in the ripple.

The JUCE-8 path (`FxVisualizer.cpp:108-145`):

```cpp
vblank_listener_ = std::make_unique<juce::VBlankAttachment>(this, [this](double ts) {
    if (!isShowing()) return;
    static double last_frame_time = 0.0;                 // function-local static!
    constexpr double fps_interval = 1.0 / 30.0;
    if (ts - last_frame_time >= fps_interval) {
        last_frame_time = ts;
        if (FxController::getInstance().isAudioProcessing()) { update(); repaint(); }
        else { reset(); repaint(); vblank_listener_.reset(); }   // self-destructs
    }
});
```

Note the `static` is shared process-wide and the listener **destroys itself from inside
its own callback** when audio stops. Do not port that structure; use
`ctx.request_repaint_after(Duration::from_nanos(33_333_333))`.

`enablementChanged()` → `start()` when enabled, `pause(); reset();` when disabled
(`FxVisualizer.cpp:223-234`). `lookAndFeelChanged()` → `calcGradient(); repaint();`
(`:236-240`). `FxController::timerCallback` calls
`main_window_->startVisualizer()` / `pauseVisualizer()` on the 5-tick audio-activity
edges (`FxController.cpp:2082-2099`).

## B6. The bar history ripple — exact algorithm

`FxVisualizer::update()` — `FxVisualizer.cpp:172-194` (in-file `:106-128`):

```cpp
for (int i = 0; i < 10; i++) {
    if (band_values_[i] < 0 || band_values_[i] > 1) band_values_.set(i, 0);
    for (int j = 0; j < NUM_BARS/2; j++) {               // j = 0..4
        band_graph_[i*10 + j]          = band_graph_[i*10 + j + 1];
        band_graph_[i*10 + (9 - j)]    = band_graph_[i*10 + j + 1];
    }
    band_graph_[i*10 + 5] = band_values_[i];
}
```

**[derived]** unrolled, reading old values (writes hit indices 0,9,1,8,2,7,3,6,4,5 in
that order, reads hit 1,2,3,4,5 — so every read is of an un-clobbered value except the
harmless `g[5] = g[5]` at `j = 4`):

```
new g[0] = old g[1]      new g[9] = old g[1]
new g[1] = old g[2]      new g[8] = old g[2]
new g[2] = old g[3]      new g[7] = old g[3]
new g[3] = old g[4]      new g[6] = old g[4]
new g[4] = old g[5]      new g[5] = old g[5]  →  overwritten by the new value
```

Resolving the recurrence, with `v_n` = this frame's band value:

| bar index | 0 | 1 | 2 | 3 | 4 | **5** | 6 | 7 | 8 | 9 |
|---|---|---|---|---|---|---|---|---|---|---|
| shows | `v_{n-5}` | `v_{n-4}` | `v_{n-3}` | `v_{n-2}` | `v_{n-1}` | **`v_n`** | `v_{n-2}` | `v_{n-3}` | `v_{n-4}` | `v_{n-5}` |

So each band is drawn as a **10-bar mirrored history**: newest in slot 5, rippling
outward in both directions, **and the mirror is off by one** — slot 4 shows `v_{n-1}`
while slot 6 shows `v_{n-2}`. Reproduce this asymmetry; it is what makes the animation
look like it flows left-to-right.

`reset()` zeroes all 100 entries (`FxVisualizer.cpp:164-170`).

## B7. Rendering — exact geometry

`FxVisualizer::paint()` — `FxVisualizer.cpp:196-221` (in-file `:130-155`):

```cpp
g.setFillType(ControlBackground @ alpha 1.0);
g.fillRoundedRectangle(bounds.toFloat(), 8);            // 960 x 120, radius 8

g.setGradientFill(gradient_);

Path barsPath;
float x  = 27;
float dx = 9.1;                                          // double → float narrowing
for (int i = 0; i < 100; i++) {
    float band_value = band_graph_[i] == 0.0 ? 0.01 : band_graph_[i];
    float height     = band_value * 100.0f;
    barsPath.addRectangle(x, 120/2.0f - height/2.0f, 4.0f, height);
    x += dx;
}
g.fillPath(barsPath);                                    // ONE path, ONE gradient
```

**[derived]** geometry:

| Quantity | Value |
|---|---|
| First bar left edge | `x = 27.0` |
| Bar width | `4.0` |
| Bar pitch | `9.1` |
| Gap between bars | `9.1 − 4.0 = 5.1` |
| Last (100th) bar left edge | `27 + 99 × 9.1 = 927.9` |
| Last bar right edge | `931.9` |
| Right margin | `960 − 931.9 = 28.1` (vs 27 on the left) |
| Bar vertical centre | `60.0` |
| Max bar height (value 1.0) | `100.0` → spans y `10 .. 110` |
| Floor bar height (value 0) | `1.0` (the `0 → 0.01` substitution) → y `59.5 .. 60.5` |

```
FxVisualizer 960 x 120, ControlBackground rounded r=8
 x:0    27                                                       931.9   960
 y:0  ┌──────────────────────────────────────────────────────────────────┐
 10   │        ▐▌                                                        │  GraphHigh
      │   ▐▌   ▐▌  ▐▌                            ▐▌                      │
 60   │ ▐▌▐▌▐▌▐▌▐▌▐▌▐▌▐▌ … 100 bars, 4 wide, 9.1 pitch …  ▐▌▐▌▐▌▐▌        │  GraphLow
      │   ▐▌   ▐▌  ▐▌                            ▐▌                      │
110   │        ▐▌                                                        │  GraphHigh
120   └──────────────────────────────────────────────────────────────────┘
        band 0 ─┬─ 10 bars ─┘ band 1 ─┬─ 10 bars ─┘ …  band 9
```

Each group of 10 consecutive bars belongs to one spectrum band; band `k` occupies bars
`10k .. 10k+9`, i.e. x `27 + 91k … 27 + 91k + 94` **[derived]** (91 px per band group,
the last bar of the group ending 94 px after the group start).

## B8. The gradient

`FxVisualizer::calcGradient()` — `FxVisualizer.cpp:242-262` (in-file `:176-196`):

```cpp
float alpha = 0.75;
if (FxController::getInstance().isAudioProcessing()) alpha = 1.0;

gradient_ = ColourGradient(GraphHigh_or_grey @ alpha, 2.0f, 0.0f,
                           GraphHigh_or_grey @ alpha, 2.0f, 100.0f, /*radial=*/false);
gradient_.addColour(0.5f, GraphLow_or_grey @ alpha);
```

| Stop | y (component space) | Colour |
|---|---|---|
| 0.0 | 0 | `GraphHigh` |
| 0.5 | 50 | `GraphLow` |
| 1.0 | 100 | `GraphHigh` |

Beyond y = 100 (the component is 120 tall) JUCE clamps to the end colour
**[JUCE semantics]** — so rows 100..120 are `GraphHigh`. Bars never reach below
y = 110 anyway.

* Alpha: **1.0** while audio is processing, **0.75** otherwise.
* Disabled (`!isEnabled()`): both stop colours are `withSaturation(0.0f)` — see the
  grey table in §A8. Dark: `#d5d5d5` ends / `#fefefe` middle. Light: `#ffffff`
  throughout (both light-mode graph colours desaturate to pure white — **a real
  legibility bug in light mode when powered off**).
* The gradient is in **component space**, shared by all 100 bars — it is *not* per-bar.
  Short bars therefore sample only the middle of the ramp (`GraphLow`), tall bars span
  the whole thing (`GraphHigh` at the tips → `GraphLow` at the waist → `GraphHigh` at
  the base).
* `calcGradient()` is called from the constructor, `start()`, `pause()`,
  `lookAndFeelChanged()` and `FxProView::update()` (`FxProView.cpp:61`).

## B9. Rust / egui render loop for the visualizer

```rust
const VIS: Vec2 = vec2(960.0, 120.0);
const NUM_BANDS: usize = 10;
const NUM_BARS:  usize = 10;
const BAR_X0: f32 = 27.0;
const BAR_DX: f32 = 9.1;
const BAR_W:  f32 = 4.0;

pub struct Visualizer {
    graph: [f32; NUM_BANDS * NUM_BARS],   // 100
}

impl Visualizer {
    pub fn reset(&mut self) { self.graph = [0.0; 100]; }

    /// Call at 30 Hz with the latest 10 band levels (0.0 ..= 1.0).
    pub fn push(&mut self, bands: &[f32; NUM_BANDS]) {
        for i in 0..NUM_BANDS {
            let v = if !(0.0..=1.0).contains(&bands[i]) { 0.0 } else { bands[i] };
            let g = &mut self.graph[i * NUM_BARS .. i * NUM_BARS + NUM_BARS];
            // exact unrolled form from §B6 — note g[4] and g[6] differ
            let (o1, o2, o3, o4, o5) = (g[1], g[2], g[3], g[4], g[5]);
            g[0] = o1; g[9] = o1;
            g[1] = o2; g[8] = o2;
            g[2] = o3; g[7] = o3;
            g[3] = o4; g[6] = o4;
            g[4] = o5; g[5] = v;
        }
    }

    pub fn ui(&self, ui: &mut egui::Ui, theme: &Theme, enabled: bool, processing: bool) {
        let (rect, _) = ui.allocate_exact_size(VIS, egui::Sense::hover());
        let p = ui.painter_at(rect);
        let o = rect.min.to_vec2();

        p.rect_filled(rect, 8.0, theme.control_background);

        let a  = if processing { 1.0 } else { 0.75 };
        let hi = if enabled { theme.graph_high } else { desaturate(theme.graph_high) };
        let lo = if enabled { theme.graph_low  } else { desaturate(theme.graph_low)  };
        // component-space gradient: hi @ y=0, lo @ y=50, hi @ y=100, clamped past 100
        let col_at = |y: f32| -> Color32 {
            let t = (y / 100.0).clamp(0.0, 1.0);
            let c = if t < 0.5 { lerp_srgb(hi, lo, t * 2.0) }
                    else       { lerp_srgb(lo, hi, (t - 0.5) * 2.0) };
            c.linear_multiply(a)
        };

        // 100 bars as a single Mesh so each bar carries the vertical gradient
        let mut mesh = egui::Mesh::default();
        let mut x = BAR_X0;
        for i in 0..(NUM_BANDS * NUM_BARS) {
            let v = if self.graph[i] == 0.0 { 0.01 } else { self.graph[i] };
            let h = v * 100.0;
            let (y0, y1) = (60.0 - h / 2.0, 60.0 + h / 2.0);
            push_v_gradient_quad(&mut mesh,
                egui::Rect::from_min_max(pos2(x, y0) + o, pos2(x + BAR_W, y1) + o),
                col_at(y0), col_at(y1));
            x += BAR_DX;                       // accumulate, do not multiply
        }
        p.add(egui::Shape::mesh(mesh));

        ui.ctx().request_repaint_after(std::time::Duration::from_nanos(33_333_333));
    }
}
```

`push_v_gradient_quad` emits 4 vertices with the top pair coloured `col_at(y0)` and the
bottom pair `col_at(y1)` — a linear interpolation across a ≤ 100 px quad is
indistinguishable from the true stop-wise ramp because the two segments of the ramp are
themselves linear and a bar never straddles the midpoint asymmetrically enough to
matter. If you want bit-exactness, split each quad at y = 50.

Accumulate `x += 9.1_f32` rather than computing `27.0 + i as f32 * 9.1` — float
accumulation drift is what the original does, and the two differ by ~1e-4 px by bar 100.

---

## C. Windows machinery → Linux / Wayland / PipeWire substitutions

| Windows mechanism (what it achieves) | Where | Linux substitute |
|---|---|---|
| `juce::VBlankAttachment` — repaint synced to the display's vblank | `FxVisualizer.cpp:115` | `egui::Context::request_repaint_after(33.3 ms)`. On Wayland, `winit` already throttles to the compositor's frame callbacks; do **not** spin. For a true vsync tie, use `eframe` with the wgpu backend and `PresentMode::Fifo`. |
| `AnimatedAppComponent::setFramesPerSecond(30/10)` | `FxVisualizer.cpp:101,143,160` | Two repaint intervals: 33.3 ms while audio flows, 100 ms idle (or stop repainting entirely and wake on new data). |
| Spectrum values crossing the driver↔GUI boundary via `dfxSharedUtil` shared memory | `dfxpSpectrum.cpp:162`, `dsp/ptutil/dfxSharedUtil/dfxSharedUtil.cpp:174` | If DSP and UI are one process: a `triple_buffer` or `[AtomicU32; 10]` of bit-cast f32s written from the PipeWire `on_process` callback. If split: a PipeWire **filter-chain node** publishing levels as a custom SPA param, or a `memfd` ring buffer. Never lock in the RT callback. |
| `WM_HOTKEY`, `HWND`, registry-backed `EQOn` flag | `FxController.cpp:1923`, `dsp/DfxDspEq.cpp:70-82` | No registry: persist the EQ on/off flag and band layout in `$XDG_CONFIG_HOME/fxsound/config.toml`. Global hotkeys are **not available to a Wayland client** — delegate to the compositor (a user-defined binding invoking a D-Bus method on the app), or expose MPRIS / a D-Bus interface the compositor can call. |
| `AttachConsole(ATTACH_PARENT_PROCESS)` for CLI status output | `FxController.cpp:684` | Plain `stdout`; nothing special needed. |
| `Drawable::createFromImageData(Slider_Thumb.svg)` | `FxTheme.cpp:98-102` | Rasterize the SVG once at startup with `resvg`/`usvg` + `tiny-skia` into an `egui::TextureHandle`, or draw `Shape::circle_filled` + a 1 px ring — the asset is a small round knob. Rasterize per `pixels_per_point` for HiDPI. |
| Gilroy SemiBold / Bold embedded via `BinaryData` | `FxTheme.cpp:386-388` | Gilroy is commercially licensed — do **not** redistribute. Ship a metrically-similar libre face (Manrope or Inter) via `egui::FontDefinitions::font_data`, and re-check every fixed pixel width in §A7 (labels are the only text with a hard budget: 74 px for `"16.00 kHz"` at 12 px). |
| `juce::Colour(uint32)` = ARGB with implicit alpha 0 | `FxTheme.cpp:22-29` | Store as `Color32::from_rgb(r,g,b)` and apply the documented alpha explicitly at each site. |
| `Colour::withSaturation(0.0f)` | `FxEqualizer.cpp:321-323`, `FxVisualizer.cpp:250-260` | `fn desaturate(c) -> Color32 { let m = r.max(g).max(b); Color32::from_rgba_unmultiplied(m,m,m,a) }` — matches JUCE's HSB round-trip exactly **[derived]**. |
| `ColourGradient` brush | `FxEqualizer.cpp:391`, `FxVisualizer.cpp:250` | egui has no gradient brush: emit `egui::Mesh` with per-vertex colours (shown in §A18/§B9). |
| `MouseCursor::PointingHandCursor` | `FxEqualizer.cpp:400,432,500,550` | `ui.ctx().set_cursor_icon(CursorIcon::PointingHand)` inside the hover branch. |
| `TooltipWindow` with custom bounds/padding | `FxTheme.cpp:515-546` | `Response::on_hover_text` / `show_tooltip_at_pointer`; restyle `Visuals::window_fill` to match `#0f0f0f`/`#e0e0e0` and the ±18/36 px offsets if you want pixel parity. |
| Right-click as "reset control" | `FxEqualizer.cpp:484`, `:596` | `Response::secondary_clicked()` — works identically under Wayland. Consider also offering a modifier (some Wayland desktops steal right-click for gestures on touch). |
| `ModifierKeys::getCurrentModifiersRealtime().isAltDown()` for solo mode | `FxEqualizer.cpp:130` | `ui.input(\|i\| i.modifiers.alt)`. **Caution:** Alt+drag is a window-move gesture in GNOME/KDE by default — offer a second binding (e.g. Ctrl+Alt or a long-press) and document it. |
| Per-monitor DPI via JUCE desktop scaling | — | `egui`'s `pixels_per_point`; keep all the constants in this document as **points**, not pixels, and let egui scale. Wayland fractional scaling arrives via `wp_fractional_scale_v1` through `winit`. |
| Audio tap for the spectrum (virtual soundcard passthrough) | `audiopassthru/` | A PipeWire filter node inserted on the sink, or a `pw-stream` capturing the sink's **monitor** port. Compute the 10 band levels in `on_process` (the filter chain is already RT-safe: 10 biquads × 2 channels is trivial), publish lock-free. Do not allocate or lock there. |

---

## Open questions / risks for the Rust port

1. **`printf("%.0f", 62.5)` rounding mode.** MSVC's CRT rounds half away from zero
   (`"63"`); glibc rounds half to even (`"62"`). Band 1 of the 10-band EQ has centre
   62.5 Hz exactly (`GraphicEqSet.cpp:449`) and band 2 of the 20-band EQ is 31.5 Hz.
   Decide explicitly — I recommend matching MSVC (`format!("{:.0}", …)` in Rust rounds
   half away from zero, so Rust's default already matches Windows). Verify against a
   real FxSound screenshot before shipping.

2. **The spectrum filters are designed for 44.1 kHz but run at the internal rate.**
   `spectrumReset.cpp:118-165` hard-codes `a1`/`a2` and never recomputes them, while
   `internal_samp_freq` is 48 kHz on most Linux systems (`spectrumReset.cpp:73`). Band
   centres shift up ~8.8 % and the top band's Nyquist-adjacent response changes shape.
   Port as-is for visual parity, or re-design the bandpasses per rate and accept a
   slightly different-looking meter. **My recommendation: re-design properly** — the
   visualizer is decorative and nobody will diff it, but a 48 kHz-correct meter is
   strictly better.

3. **`band_boosts_[1]` is dereferenced unconditionally** at `FxEqualizer.cpp:391`, and
   `band_boosts_.size() - 1` at `:350` underflows on an empty vector. The DSP allows
   `num_bands = 1` (`GraphicEqSet.cpp:128`, `GraphicEqSet.cpp:386-392`). The UI combo
   never offers it, but `setNumEqBands` has no validation
   (`FxController.cpp:1778-1782`) and the JSON/CLI path can reach it
   (`FxController.cpp:293`, `:472`). Decide: clamp the band count to the five supported
   values, or make the curve drawing handle N ∈ {1, 2}.

4. **N = 31 overlapping hit areas.** The 32 px slider is wider than the 24 px column
   (§A7) — 8 px of every boundary is ambiguous. Reproducing JUCE's "last child wins"
   is possible but feels wrong; I recommend clamping the interactive width to
   `min(32, col_w)` and documenting the divergence.

5. **Right-click frequency reset for band counts other than 5/10/15+** uses hard-coded
   20 Hz–20 kHz endpoints and an `int` truncation (`FxEqualizer.cpp:637-639`), which
   can produce a value outside the band's own range and be silently discarded
   (`FxController.cpp:1855-1858`). Since only {5,10,15,20,31} are reachable from the
   UI, the branch is effectively dead — but if the Rust port exposes arbitrary band
   counts, fix it to use the band's real `min_band_freq`/`max_band_freq`.

6. **The frequency-range boundaries are derived from the generic log grid, not from the
   ISO centre tables** (§A4). For N = 15/20/31 several bands' default centres sit
   off-centre inside their own range (e.g. N=20 band 2: centre 31.5 in range 25..34,
   which is fine; N=5 band 1: centre 62.5 at the very bottom of 62.5..125, which looks
   broken on the wheel). Preserving this is necessary for preset compatibility.
   Consider showing the wheel's value textually (already done) so the off-centre knob
   reads as intentional.

7. **Light-mode disabled visualizer is invisible.** Both `GraphHigh` (`#1ac1ff`) and
   `GraphLow` (`#72d8ff`) desaturate to `#ffffff` (§A8), drawn on `ControlBackground`
   `#e0e0e0` at alpha 0.75. Contrast ratio ≈ 1.3:1. Fix in the port (use a mid grey),
   and note the same issue affects the EQ curve's `EqStart` in light mode.

8. **`FxEqualizer` polls `getNumEqBands()` from inside `paint()` and calls `reinit()`
   there** (`FxEqualizer.cpp:303-308`), mutating the component tree during painting.
   This is a latent crash in JUCE and is impossible to express in egui anyway. Restructure
   as: band count is model state; the view rebuilds derived layout each frame from it.

9. **No peak-hold, no explicit decay in the GUI.** If product expectations include a
   peak-hold cap (most spectrum analyzers have one), it is a *new feature*, not a port.
   Get that decided before implementing — adding one changes §B6 entirely.

10. **The 25 Hz DSP store vs 30 Hz GUI poll** (§B5) means ~1 in 6 frames duplicates a
    value and the history ripple shows it twice. Matching the GUI to 25 Hz would look
    subtly different from Windows. I recommend driving the Rust `push()` from *new data
    arrival* (one push per DSP update, 25 Hz) and repainting at display rate, which is
    strictly better and removes the duplicate.

11. **`Path::addLineSegment(l, 1.0)` + `strokePath(1.0)` produces ~2 px of ink with a
    hollow core** (§A9). Any single-stroke egui approximation will differ slightly at
    steep slopes. Pick 1.5 px and compare screenshots; do not spend effort emulating
    the hollow core.

12. **Gilroy licensing** (§C). The repo ships `GilroyRegular.ttf` / `GilroySemibold.ttf`
    / `GilroyBold.ttf` as `BinaryData`. Confirm redistribution rights before embedding
    anything in the Linux build; otherwise substitute and re-measure the 74 px / 49 px /
    24 px label columns.

13. **Alt+drag conflicts with the compositor** (§C). GNOME and KDE both bind Alt+drag to
    window move by default. The solo feature is undiscoverable as-is; either rebind or
    surface it in the UI (e.g. a small "solo" affordance per band).

14. **`FxEqualizer::getInstance()` singleton vs the `FxProView` member** — the singleton
    at `FxEqualizer.h:32-36` is never used. If any future code path (CLI, tray menu) was
    meant to reach the EQ through it, that intent is lost. Confirm before designing the
    Rust ownership model; the safe default is a single owned `EqState` in the app struct.

15. **Unverified: the exact `host_buffer_delay_msecs`** that feeds
    `spectrumSetDelay` (`dfxpSpectrum.cpp:56`) — it comes from the Windows driver's
    buffer size and I did not trace its origin. On PipeWire, use the node's actual
    quantum/latency (`pw_stream` → `SPA_PARAM_Latency`) to compute `delay_count`, or
    just set it to 0 and accept the meter leading the audio by one quantum.
