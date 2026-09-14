# 11 - Presets and the `.fac` file format

**Scope.** Everything the Rust/egui Linux port needs in order to (a) read every `.fac`
preset that the Windows FxSound 1.1.x ships or that a user has ever saved, (b) write
files that the Windows application can read back, and (c) reproduce FxSound's preset
lifecycle (factory / user / auto-save / import / export / rename / delete).

Every number below was read out of the source tree at
`/home/blackixxce/Загрузки/fxsound-app-main` and is cited as `path:line`.

---

## 0. Executive summary - the one thing to get right first

**`.fac` is not a binary format.** Despite the task framing and the "Factory preset"
extension, a `.fac` file is a **line-oriented, 7-bit-ASCII-plus-UTF-8 text file** written
with `fwprintf`/`fprintf` and parsed with `fgetws`/`swscanf`. There is:

* **no magic number** in the binary sense - the identifying token is the literal ASCII
  string `CLASS1` at the start of line 1 (`dsp/ptutil/VALS/Valsfile.cpp:546-557`);
* **no checksum, no CRC, no length prefix, no padding, no endianness** anywhere;
* **no fixed byte offsets** - the format is *positional by line*, and the line lengths
  change with the magnitude of the numbers they carry.

Each line has the shape

```
<value>: <human-readable label>
```

and the reader **only ever parses the leading token**; the label after the colon is
decoration that the reader skips entirely. Two line kinds are pure label lines with no
value at all (`Band N` and `String[i]:`) and the reader consumes-and-discards them by
position. This is what makes the format fragile: *insert or delete a single line and
everything after it is misinterpreted with no error.*

---

## 1. Where the format lives in the C++ tree

| Concern | File | Notes |
|---|---|---|
| Serialiser (`valsSave`) | `dsp/ptutil/VALS/Valsfile.cpp:45-187` | the definitive grammar |
| Deserialiser (`valsRead`) | `dsp/ptutil/VALS/Valsfile.cpp:266-507` | the definitive parse order |
| Type sniffer (`valsCheckFileType`) | `dsp/ptutil/VALS/Valsfile.cpp:514-563` | checks first token == `CLASS1` |
| In-memory model (`struct valsHdlType`) | `dsp/ptutil/VALS/U_vals.h:55-95` | |
| Array sizes | `dsp/ptutil/include/vals.h:23-25` | `6` main, `7` element, `8` max elements |
| Accessors | `dsp/ptutil/VALS/Valsget.cpp`, `Valsset.cpp` | |
| Preset semantics (vals -> DSP state) | `dsp/DfxDspPreset.cpp:140-266` | `getStateInfoFromVals()` |
| Preset semantics (DSP state -> vals) | `dsp/DfxDspPreset.cpp:268-361` | `createValsFromStateInfo()` |
| Public entry points | `dsp/DfxDspPreset.cpp:58-135` | `loadPreset`/`savePreset`/`exportPreset` |
| Name-only probe | `dsp/DfxDspPreset.cpp:363-397` | `getPresetInfo()` |
| EQ block payload | `dsp/ptutil/DspUtil/GraphicEq/*` | band CF + boost/cut arrays |
| Preset slot bookkeeping (legacy, dead) | `dsp/ptutil/PRELST/Prelst.cpp` | numbered-slot scheme, **not used by the JUCE app** |
| GUI lifecycle | `fxsound/Source/GUI/FxController.cpp:803-876, 1204-1458` | |
| GUI model | `fxsound/Source/GUI/FxModel.h:34-40`, `FxModel.cpp:55-153` | |

### 1.1 The PRELST module is legacy - do not port it

`dsp/ptutil/PRELST/Prelst.cpp` implements an older scheme where presets live in fixed
numbered slots, file-named `"<index+1>.fac"`:

* `prelstConstructFilename()` (`Prelst.cpp:172-190`) formats `L"%d.%s"` with
  `i_index + 1` and either `PRELST_FACTORY_EXTENSION_WIDE` or `PRELST_USER_EXTENSION_WIDE`;
* `prelstAskIsFactory()` (`Prelst.cpp:197-215`) classifies index `< user_min_index` as
  factory, else user;
* `prelstConstructFullpath()` (`Prelst.cpp:138-165`) joins with a **backslash**:
  `swprintf(wcp_fullpath, L"%s\\%s", dir, filename)`;
* `prelstCreate()` (`Prelst.cpp:47-109`) walks `index = 0 ..= i_user_max_index`, calls
  `valsCheckFileType()` on each candidate path and records an `exist_flags[]` bitmap;
* `prelstCalcListToRealNum()` / `prelstCalcRealToListNum()` (`Prelst.cpp:225-301`) convert
  between the sparse slot index and the dense list-box row index;
* `prelstNextAvailableUserPreset()` (`Prelst.cpp:353-383`) finds the first free user slot
  and errors with `"No available preset locations."` / `"You must delete a preset."`
  when full.

The slot boundary was `DFXG_MIN_USER_PRESET_INDEX = 99` - "0 based number of min user
preset (preset number 100)" (`dsp/DfxDspPrivate.cpp:42`). The numbered `1.fac` ...
`12.fac` files still shipped in `Installer/Resources/Factsoft/` are the fossil of this
scheme.

**The shipping JUCE app abandoned it.** `DfxDspPrivate::preset_list_handle_` is only ever
freed, never created (`dsp/DfxDspPrivate.cpp:124-128`), and the `initPresets()` that would
call `prelstCreate()` is commented out wholesale (`dsp/DfxDspPreset.cpp:500-547`). The live
code enumerates presets by directory glob instead (§6). **The Rust port should implement
the glob scheme and drop PRELST entirely.**

---

## 2. The grammar, line by line

Written by `valsSave()` (`Valsfile.cpp:45-187`), read by `valsRead()` (`Valsfile.cpp:266-507`).
`V` = file version (§3.1), `E` = `total_num_elements`, `D` = `double_params`,
`NI`/`NR`/`NS` = app-dependent counts, `NB` = number of EQ bands.

```
line  1                 : "CLASS1 : Effect Type"                       <- literal, Valsfile.cpp:76
line  2                 : "<V>: Version"                               <- %g,     Valsfile.cpp:78
line  3                 : "<preset name>"                              <- raw UTF-8 bytes, Valsfile.cpp:87
line  4   (iff V > 1.0) : "<D>: Double Params Flag"                    <- %d,     Valsfile.cpp:93-96
line  5                 : "<E>: Total number of elements"              <- %d,     Valsfile.cpp:98
          repeat 6x (VALS_NUM_MAIN_PARAMS), i = 0..5 :
            "<main[i]>: Main <i>"                                      <- %d,     Valsfile.cpp:103
            iff D: "<main2[i]>: Main_2 <i>"                            <- %d,     Valsfile.cpp:107
          repeat E times, e = 0..E-1 :
            "<e>: Element Number"                                      <- %d,     Valsfile.cpp:116
            repeat 7x (VALS_NUM_ELEMENT_PARAMS), p = 0..6 :
              "   <elem[e][p]>: Param <p>"                             <- 3sp+%d, Valsfile.cpp:120
              iff D: "   <elem2[e][p]>: Param_2 <p>"                   <- 3sp+%d, Valsfile.cpp:125
                    : "<NI>: Number of Application Dependent Integers" <- %d,     Valsfile.cpp:133
                    : "<NR>: Number of Application Dependent Reals"    <- %d,     Valsfile.cpp:135
                    : "<NS>: Number of Application Dependent Strings"  <- %d,     Valsfile.cpp:137
          repeat NI times, i :
            "<int_vals[i]>: Integer[<i>]"                              <- %d,     Valsfile.cpp:142
          repeat NR times, i :
            "<real_vals[i]>: Real[<i>]"                                <- %g,     Valsfile.cpp:148
          repeat NS times, i :
            "String[<i>]:"                                             <- label only, Valsfile.cpp:154
            "<string i, or empty line if NULL>"                        <- Valsfile.cpp:156-158
          iff a Graphic-EQ handle exists (writer) / iff V >= 9 (reader) :
                    : "<NB>: Number of EQ Bands"                       <- %d,     Valsfile.cpp:167
                    : "<eq_on>: On/Off Flag"                           <- %d,     Valsfile.cpp:169
          repeat NB times, b = 1..NB :
            "Band <b>"                                                 <- label only, Valsfile.cpp:178
            "   <center_freq_hz>: CF"                                  <- 3sp+%g, Valsfile.cpp:179
            "   <boost_cut_db>: Boost/Cut"                             <- 3sp+%g, Valsfile.cpp:180
EOF
```

### 2.1 Reader-side quirks that a conformant Rust parser must replicate

| Quirk | Evidence | Consequence for the port |
|---|---|---|
| Line 1 is read and **thrown away** by `valsRead` | `Valsfile.cpp:312` | only `valsCheckFileType` validates it |
| Validation = `swscanf(L"%s")` on line 1, compared to `L"CLASS1"` | `Valsfile.cpp:546-557` | leading whitespace tolerated; only the first whitespace-delimited token matters |
| Version parsed with `%g` into a `float` | `Valsfile.cpp:316` | `"9"`, `"9.0"`, `"9.00"` are all version 9 |
| Comment read with **narrow** `fgets` then UTF-8 decoded | `Valsfile.cpp:320-323` | the name is UTF-8 **bytes**, not UTF-16 |
| Comment: last character is unconditionally overwritten with NUL | `Valsfile.cpp:327-331` | on a CRLF file read in binary this would eat the `\r`; in MSVC text mode it eats the `\n`. **A byte-exact port must strip a trailing `\r\n` or `\n`, then treat an already-empty line as "no name".** |
| Empty comment line -> `wcp_comment = NULL` | `Valsfile.cpp:342-343` | `getPresetInfo()` then returns an empty name, and `FxController::initPresets()` **skips the file** (`FxController.cpp:850, 862`) |
| `double_params` line only present when `V > 1.0` | `Valsfile.cpp:350-358` | pre-2.0 files have one fewer line |
| Element index line must equal the loop counter or the whole read fails | `Valsfile.cpp:386-389` | strict |
| `%d` on `"   0: Param 0"` | `Valsfile.cpp:394` | C `%d` skips leading blanks, stops at `:` |
| EQ block read only when `V >= 9` | `Valsfile.cpp:453` | for `V < 9` `hp_graphicEq` stays `NULL` |
| `LINE_LENGTH 128` for every `fgetws`/`fgets` | `Valsfile.cpp:38` | a line longer than 127 chars is **split** and desynchronises the parse. Preset names are capped at `DFXG_MAX_PRESET_NAME_LENGTH = 128` (`dsp/DfxDspPrivate.cpp:43`) |
| `valsRead` leaks the handle and returns `NOT_OKAY` on malformed input without closing the stream in several paths | `Valsfile.cpp:389, 415` | do not copy this; return `Result` |
| `getPresetInfo()` ignores every error return | `dsp/DfxDspPreset.cpp:363-397` | it will happily dereference a `NULL` `vals_hdl`. Rust port must return `Result` |

### 2.2 Writer-side quirks

| Quirk | Evidence |
|---|---|
| File opened `L"w"` - **text mode on Windows**, so `\n` becomes CRLF there | `Valsfile.cpp:72` |
| Path joined with a literal backslash: `swprintf(fullpath_str, L"%s\\%s", dir, filename)` | `Valsfile.cpp:69` |
| Everything except the name is written with **wide** `fwprintf`; the name alone with **narrow** `fprintf` of a UTF-8 buffer | `Valsfile.cpp:76-87` |
| A `NULL` comment writes a bare empty line | `Valsfile.cpp:80-81` |
| The EQ block is emitted **iff `hp_graphicEq != NULL`**, independent of version | `Valsfile.cpp:162` |
| Floats use `%g` -> 6 significant digits, trailing zeros stripped, `e+NN` above 1e6 | `Valsfile.cpp:78, 148, 179, 180` |
| `savePreset()` appends `L".fac"` to the preset name to form the filename | `dsp/DfxDspPreset.cpp:106` |

**Observed line endings in this tree** (measured, `python3` over all 32 shipped files):
13 of 19 `bin/BonusPresets/*.fac` are **LF**, 6 are **CRLF**
(`Bass (Quizal)`, `Bass - ambience (Quizal)`, `Conotating Life (Quizal)`,
`Panserotaliya (Quizal)`, `Quizal Star`, `Underworld Life (Quizal)`,
`Yoznogarda dy flybe - Life (Quizal)` - 7 CRLF in total).
All 13 `Installer/Resources/Factsoft/*.fac` are LF, and **`1.fac` has no final newline
at all** (884 bytes, last line `   13000: Boost/Cut` unterminated). A conformant reader
must therefore accept LF, CRLF, and a missing final terminator.

---

## 3. Field semantics

### 3.1 Version (`file_version`, a `float`)

| Value | Written by | Meaning |
|---|---|---|
| `<= 1.0` | pre-historic | no "Double Params Flag" line at all (`Valsfile.cpp:93, 350`) |
| `< 3.0` | pre-DFX-bass | bass-boost fields absent -> reader forces `button_on = IS_TRUE`, `i_value = 0` (`dsp/DfxDspPreset.cpp:206-210`) |
| `< 4.0` | pre-headphone | headphone field absent -> reader forces `headphone_on_ = IS_FALSE` (`dsp/DfxDspPreset.cpp:226-229`) |
| `7.0` | "PRE DFX 12" | comment at `dsp/DfxDspPreset.cpp:50` |
| `9.0` | **DFX 12+ / current** | `DFXG_VALS_FILE_VERSION 9.0` (`dsp/DfxDspPreset.cpp:53`); EQ block present (`Valsfile.cpp:453`) |

Every one of the 32 `.fac` files shipped in this tree is version `9`.
`valsInit()` is always called with `DFXG_VALS_FILE_VERSION` (`dsp/DfxDspPreset.cpp:278`),
so the Rust writer must emit exactly `9: Version`.

### 3.2 `double_params`

`0` in every shipped file. When `1`, a mirror value line follows *every* main and element
param (`Valsfile.cpp:105-109, 123-128`), doubling the body. The DFX preset path never sets
it - `valsInit(..., IS_FALSE)` at `dsp/DfxDspPreset.cpp:278`. **Parse it, never emit it as
`1`.**

### 3.3 `total_num_elements` and the element block

`valsInit(hpp_vals, slout1_, 1, DFXG_VALS_FILE_VERSION, IS_FALSE)` - the literal `1`
(`dsp/DfxDspPreset.cpp:278`) is the element count. Hard caps:
`VALS_NUM_MAIN_PARAMS 6`, `VALS_NUM_ELEMENT_PARAMS 7`, `VALS_MAX_NUM_ELEMENTS 8`
(`dsp/ptutil/include/vals.h:23-25`).

The one element's 7 params are **all zero in every shipped preset** and are never read by
any DFX code path - `valsGetElementParamValue()` (`Valsget.cpp:198-221`) has no caller in
`DfxDspPreset.cpp`. They are a vestige of the multi-element delay/reverb engine the `vals`
module was originally written for (note `maxSum1_4` / `factor_1_to_4` in
`U_vals.h:82-87`, and the delay-oriented `vals_CalcNewMaxSum1and4()` at
`Valsset.cpp:294-388`). **The Rust port must still round-trip these 7 zeros verbatim.**

### 3.4 Main params - the 6 MIDI knob values

> **The index map is not contiguous.** `Main 2` is a hole.

| Line | Constant | Value | Effect | Source |
|---|---|---|---|---|
| `Main 0` | `DFXG_VALS_FIDELITY_INDEX` | `0` | Fidelity | `dsp/DfxDspPreset.cpp:24` |
| `Main 1` | `DFXG_VALS_SURROUND_INDEX` | `1` | Surround | `dsp/DfxDspPreset.cpp:25` |
| `Main 2` | *(none)* | `2` | **unused / always 0** | - |
| `Main 3` | `DFXG_VALS_AMBIENCE_INDEX` | `3` | Ambience | `dsp/DfxDspPreset.cpp:26` |
| `Main 4` | `DFXG_VALS_DYNAMIC_BOOST_INDEX` | `4` | Dynamic Boost | `dsp/DfxDspPreset.cpp:27` |
| `Main 5` | `DFXG_VALS_BASS_BOOST_INDEX` | `5` | Bass Boost | `dsp/DfxDspPreset.cpp:28` |

Each is a **MIDI integer 0..127** (`MIDI_MIN_VALUE 0`, `MIDI_MAX_VALUE 127`,
`dsp/DfxDspPrivate.cpp:50-51`).

#### MIDI <-> real conversion (exact)

`midi_to_rval_qnt_handle_` is built with
`qntIToRInit(&h, slout, 0, 127, DFX_UI_MIN_VALUE, DFX_UI_MAX_VALUE, IS_FALSE, 0, IS_FALSE, 0.0, IS_FALSE, QNT_RESPONSE_LINEAR)`
(`dsp/DfxDspPrivate.cpp:91-97`), with `DFX_UI_MIN_VALUE 0.0` / `DFX_UI_MAX_VALUE 1.0`
(`dsp/ptutil/include/DfxSdk.h:92-93`).

Because `i_output_quantized`, `i_force_value_flag` and `i_snap_flag` are all `IS_FALSE`,
`qntIToRInit` takes the plain-linear branch at `dsp/ptutil/Qnt/Qntitor.cpp:165-175`:

```
scale            = (r_output_max - r_output_min) / (i_input_max - i_input_min)   // Qntitor.cpp:169
real_array[i]    = r_output_min + scale * i                                      // Qntitor.cpp:173
real_array[127]  = r_output_max                                                  // Qntitor.cpp:178 (hard-set endpoint)
```

and `qntIToRCalc` is a bare table lookup (`dsp/ptutil/Qnt/Qntitor2.cpp:559-581`). So:

```
value_0_to_1 = midi / 127.0                      // exact; 127 -> exactly 1.0
```

The reverse (`qntRToIInit` at `dsp/DfxDspPrivate.cpp:105-110`, `qntRToICalc` at
`dsp/ptutil/Qnt/Qntrtoi.cpp:96-97`):

```
r_scale = (127 - 0) / (1.0 - 0.0) = 127.0        // Qntrtoi.cpp:68-69
midi    = (int)((value - 0.0) * 127.0 + 0 + 0.5) // truncating cast => round-half-up
```

**Rust:**

```rust
#[inline] pub fn midi_to_value(m: u8) -> f32 { m as f32 / 127.0 }
#[inline] pub fn value_to_midi(v: f32) -> u8 { ((v * 127.0) + 0.5) as u8 }  // clamp 0..=127
```

Note the truncating cast makes `value_to_midi` round-half-up and **not** symmetric: it is
not exactly the inverse of `midi_to_value` for every input, but it *is* an exact round trip
for every value produced by `midi_to_value` (`m/127*127 + 0.5 = m + 0.5` truncates to `m`).

#### GUI scale: a factor of 10

The JUCE sliders run `0..10` in steps of `1.0` (`fxsound/Source/GUI/FxAudioControls.cpp:113`).
`DfxDsp::setEffectValue()` divides by 10 on the way in and
`DfxDsp::getEffectValue()` returns the raw `0..1` on the way out
(`dsp/DfxDspPrivate.cpp:231-305`). `FxController::setPreset()` re-applies that scaling
after every preset load:

```cpp
auto value = dfx_dsp_.getEffectValue(e);
dfx_dsp_.setEffectValue(e, value * 10);          // FxController.cpp:1087-1088
```

So the display chain is `file_midi 0..127  ->  0.0..1.0  ->  slider 0..10`.

#### The bypass flags are clobbered on load

`setEffectValue()` forces the effect button on when `value != 0.0` and off when
`value == 0.0` (`dsp/DfxDspPrivate.cpp:295-302`). Because `FxController::setPreset()`
round-trips every effect through `setEffectValue()` right after `loadPreset()`
(`FxController.cpp:1085-1089`), **the per-effect on/off flags read out of
`Integer[0..4]` are immediately overwritten by `value != 0`.** The stored flags therefore
have no observable effect in the shipping app. The Rust port should keep them in the
struct and write them back faithfully, but should treat "effect enabled" as
`midi_value != 0` to match observed behaviour. (This is visible in the data: `Jazz.fac`
stores `ambience_on = 0` *and* `Main 3 = 0`; `Metal.fac` stores `ambience_on = 0` but
`Main 3 = 38`, and the shipping app plays it as ambience-on.)

### 3.5 Application-dependent integers - exactly 7

`valsInitAppDependentInfo(*hpp_vals, DFXG_VALS_NUM_APP_DEPEND_INTS, 0, 0)` with
`DFXG_VALS_NUM_APP_DEPEND_INTS = 7` (`dsp/DfxDspPreset.cpp:31, 281`). Reals and strings are
always `0` in DFX presets, but the reader handles arbitrary counts
(`Valsfile.cpp:405-450`).

| Line | Constant | Idx | Meaning | Written at | Read at |
|---|---|---|---|---|---|
| `Integer[0]` | `DFXG_VALS_APP_DEPEND_FIDELITY_INDEX` | 0 | fidelity **on** (`!bypass`) | `DfxDspPreset.cpp:296-298` | `:158-161` |
| `Integer[1]` | `DFXG_VALS_APP_DEPEND_SURROUND_INDEX` | 1 | surround on | `:318-320` | `:182-185` |
| `Integer[2]` | `DFXG_VALS_APP_DEPEND_AMBIENCE_INDEX` | 2 | ambience on | `:307-309` | `:170-173` |
| `Integer[3]` | `DFXG_VALS_APP_DEPEND_DYNAMIC_BOOST_INDEX` | 3 | dynamic-boost on | `:329-331` | `:194-197` |
| `Integer[4]` | `DFXG_VALS_APP_DEPEND_BASS_BOOST_INDEX` | 4 | bass-boost on | `:340-342` | `:213-216` |
| `Integer[5]` | `DFXG_VALS_APP_DEPEND_HEADPHONE_INDEX` | 5 | headphone mode on | `:351-353` | `:232-233` |
| `Integer[6]` | `DFXG_VALS_APP_DEPEND_MUSIC_MODE_INDEX` | 6 | music mode | `:356-358` | *ignored* |

> **Note the index skew.** The *app-dependent* order is
> Fidelity, Surround, Ambience, DynamicBoost, BassBoost - contiguous 0..4.
> The *main-param* order is Fidelity(0), Surround(1), **hole(2)**, Ambience(3),
> DynamicBoost(4), BassBoost(5). Getting these two orders confused is the single easiest
> way to write a subtly wrong port.

Music-mode values (`dsp/DfxDspPreset.cpp:43-45`):

| Value | Constant |
|---|---|
| `1` | `DFX_UI_MUSIC_MODE_MUSIC1` |
| `2` | `DFX_UI_MUSIC_MODE_MUSIC2` |
| `3` | `DFX_UI_MUSIC_MODE_SPEECH` |

The stored value is **read and discarded**: `getStateInfoFromVals()` hard-codes
`music_mode_ = DFX_UI_MUSIC_MODE_MUSIC2;` with the comment
*"As of DFX Version 13, we only allow music mode 2"* (`dsp/DfxDspPreset.cpp:236-242`).
All 32 shipped presets store `2`. **Write `2`; ignore on read.**

`headphone_on` is `0` in all 32 shipped presets.

### 3.6 The Graphic-EQ block

Written from a `GraphicEq` handle (`Valsfile.cpp:161-182`), read back into a freshly
created one (`Valsfile.cpp:452-499`).

* `NB` comes from `GraphicEqGetNumBands()`; the handle is created by
  `GraphicEqNew(&h, i_num_bands, ...)` which rejects `i_num_bands > GRAPHIC_EQ_MAX_NUM_BANDS`
  or `< 1` (`dsp/ptutil/DspUtil/GraphicEq/GraphicEqInit.cpp:62-65`).
  `GRAPHIC_EQ_MAX_NUM_BANDS == SOS_MAX_NUM_SOS_SECTIONS == 32`
  (`dsp/ptutil/DspUtil/GraphicEq/u_GraphicEq.h:35`, `dsp/ptutil/include/sos.h:27`).
  The GUI additionally only offers `5, 10, 15, 20, 31` (`fxsound/Source/GUI/FxController.cpp:292`),
  default `DEFAULT_NUM_EQ_BANDS = 10` (`fxsound/Source/GUI/FxController.h:46`).
  **All 32 shipped presets use `NB = 10`.**
* `eq_on` is written from `dfxpEqGetProcessingOn(..., DFXP_STORAGE_TYPE_REGISTRY, &i_eq_on)`
  (`dsp/DfxDspPreset.cpp:290-292`). `1` in all 32 shipped presets.
* **Band numbering is 1-based on the wire and in the API**, 0-based in the SOS section
  array: `i_section_num = i_band_num - 1` (`GraphicEqSet.cpp:275`, `GraphicEqGet.cpp:70`).
* `CF` is a centre frequency in Hz, clamped on set to
  `GRAPHIC_EQ_MIN_BAND_FREQ = 10` .. `GRAPHIC_EQ_MAX_BAND_FREQ = 21000.0`
  (`dsp/ptutil/include/GraphicEq.h:42-43`; clamp at `GraphicEqSet.cpp:555-559`).
* `Boost/Cut` is dB, clamped on set to
  `+/- GRAPHIC_EQ_DEFAULT_MAX_BOOST_OR_CUT = 20.0`
  (`dsp/ptutil/include/GraphicEq.h:46`; clamp at `GraphicEqSet.cpp:298-302`).
  A band with exactly `0.0` dB, or whose `CF * 2 >= sampling_freq`, is set to unity gain
  and bypassed (`GraphicEqSet.cpp:288-295`).
* Read order per band is **CF line then Boost/Cut line**, but the *store* order is
  inverted - `GraphicEqSetBandBoostCut()` first, then `GraphicEqSetBandFreq()`
  (`Valsfile.cpp:494-498`) - because `SetBandFreq` internally re-applies the stored boost
  to force a coefficient refresh (`GraphicEqSet.cpp:570-575`).

#### Live (non-preset) band frequency tables

When the preset's band count does **not** match the live count, the app keeps its own
frequency table and only remaps gain (`dsp/DfxDspEq.cpp:168-227`). Those tables, from
`GraphicEqReSetAllBandFreqs()`:

| Bands | min/max | Centre frequencies (Hz) | Source |
|---|---|---|---|
| 5 | 62.5 / 16000 | 62.5, 250, 1000, 4000, 16000 | `GraphicEqSet.cpp:430-440` |
| **10** | **62.5 / 16000** | **62.5, 115.734, 214.311, 396.85, 734.867, 1360.79, 2519.84, 4666.12, 8640.48, 16000.0** | `GraphicEqSet.cpp:441-455` |
| 15 | 25 / 16000 | 25, 40, 63, 100, 160, 250, 400, 630, 1000, 1600, 2500, 4000, 6300, 10000, 16000 | `GraphicEqSet.cpp:456-467` |
| 20 | 20 / 16000 | 20, 31.5, 40, 63, 80, 125, 160, 250, 315, 500, 630, 1000, 1250, 2000, 2500, 4000, 5000, 8000, 10000, 16000 | `GraphicEqSet.cpp:468-479` |
| 31 | 20 / 20000 | 20, 25, 31.5, 40, 50, 63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600, 2000, 2500, 3150, 4000, 5000, 6300, 8000, 10000, 12500, 16000, 20000 | `GraphicEqSet.cpp:480-492` |
| other | as given | geometric: `f_i = f_min * (f_max/f_min)^(i/(n-1))` | `GraphicEqSet.cpp:493-509` |

The 10-band table is explicitly documented as the **legacy (pre-ISO) grid the factory and
community presets were authored against** (`GraphicEqSet.cpp:443-448`) - do **not**
substitute ISO 266 numbers.

Filter Q is derived, not stored:

```
r = (f_max / f_min) ^ (1 / (n - 1))
Q = sqrt(r) / (r - 1)            // GraphicEqSet.cpp:515-518
Q *= Q_multiplier                // GraphicEqSet.cpp:521 ; default Q_multiplier = 1 (GraphicEqInit.cpp:49)
Q  = max(Q, 1.0)                 // GraphicEqSet.cpp:524
```

For `n = 10`, `f_min = 62.5`, `f_max = 16000`: `r = 256^(1/9) = 1.85174`, `Q = 1.5978`.
Default sampling frequency is `GRAPHIC_EQ_DEFAULT_SAMPLING_FREQ = 44100.0`
(`GraphicEq.h:39`, applied at `GraphicEqInitSections.cpp:43`).
Default first/last band freqs before any preset load are
`GRAPHIC_EQ_DEFAULT_FIRST_BAND_FREQ 20` / `GRAPHIC_EQ_DEFAULT_LAST_BAND_FREQ 20000.0`
(`GraphicEq.h:36-37`, `GraphicEqInitSections.cpp:48-49`).

**Master gain, normalization, volume-levelling, balance and Q-multiplier are NOT stored in
the `.fac` file.** They live only in `GraphicEqHdlType` (`u_GraphicEq.h:55-58`) and in
JUCE `settings_` (`FxController.cpp:292-340`). Defaults:
`DEFAULT_VOLUME_LEVELING 0.0f`, `DEFAULT_BALANCE 0.0f`, `DEFAULT_FILTER_Q 1.0f`,
`DEFAULT_MASTER_GAIN 0.0f` (`fxsound/Source/GUI/FxController.h:48-51`), with validated
ranges volume-levelling `0..4`, balance `-20..+20`, filter Q `1..3`, master gain
`-20..+20` (`FxController.cpp:304, 316, 328, 340`). Slider steps: master gain `2`,
volume levelling `0.5`, filter Q `0.5` (`FxAudioControls.cpp:312, 330, 348`).

### 3.7 Band-count mismatch handling on load

`getGraphicEqInfoFromVals()` (`dsp/DfxDspEq.cpp:127-247`):

* `hp_graphicEq == NULL` (i.e. version `< 9`) -> force `eq_on = IS_TRUE` and flatten every
  live band to `0.0` dB (`DfxDspEq.cpp:144-158`).
* preset bands `<` live bands -> **linear interpolation**:
  `source = 1 + (i-1)*(nB-1)/(live-1)`, lerp between `floor` and `floor+1`
  (`DfxDspEq.cpp:194-209`).
* preset bands `>` live bands -> **equidistant nearest pick**:
  `source = 1 + (int)((i-1)*(nB-1)/(live-1) + 0.5)` (`DfxDspEq.cpp:214-218`).
* equal counts -> gains **and** frequencies copied verbatim (`DfxDspEq.cpp:230-241`).

`float f_bass_boost_value;` at `dsp/DfxDspEq.cpp:134` is declared and never used - dead.

---

## 4. Byte-offset table (worked example)

Because line widths are data-dependent there is no universal offset table. Here is the
real layout of `Installer/Resources/Factsoft/Default.fac` (884 bytes, LF, 61 lines),
dumped byte-exactly:

```
offset  len  content
0x0000   21  CLASS1 : Effect Type          <- magic line
0x0015   11  9: Version
0x0020    8  Default                       <- preset name (UTF-8)
0x0028   22  0: Double Params Flag
0x003E   28  1: Total number of elements
0x005A   10  0: Main 0                     <- fidelity  midi
0x0064   10  0: Main 1                     <- surround  midi
0x006E   10  0: Main 2                     <- UNUSED
0x0078   10  0: Main 3                     <- ambience  midi
0x0082   10  0: Main 4                     <- dyn boost midi
0x008C   10  0: Main 5                     <- bass      midi
0x0096   18  0: Element Number
0x00A8   14     0: Param 0                 <- 7 x element params, all 0
0x00B6   14     0: Param 1
0x00C4   14     0: Param 2
0x00D2   14     0: Param 3
0x00E0   14     0: Param 4
0x00EE   14     0: Param 5
0x00FC   14     0: Param 6
0x010A   44  7: Number of Application Dependent Integers
0x0136   41  0: Number of Application Dependent Reals
0x015F   43  0: Number of Application Dependent Strings
0x018A   14  1: Integer[0]                 <- fidelity on
0x0198   14  1: Integer[1]                 <- surround on
0x01A6   14  1: Integer[2]                 <- ambience on
0x01B4   14  1: Integer[3]                 <- dynamic boost on
0x01C2   14  1: Integer[4]                 <- bass boost on
0x01D0   14  0: Integer[5]                 <- headphone on
0x01DE   14  2: Integer[6]                 <- music mode (MUSIC2)
0x01EC   23  10: Number of EQ Bands
0x0203   15  1: On/Off Flag
0x0212    7  Band 1
0x0219   12     62.5: CF
0x0225   16     0: Boost/Cut
0x0235    7  Band 2
0x023C   13     121.5: CF
0x0249   16     0: Boost/Cut
0x0259    7  Band 3
0x0260   11     225: CF
0x026B   16     0: Boost/Cut
0x027B    7  Band 4
0x0282   13     416.5: CF
0x028F   16     0: Boost/Cut
0x029F    7  Band 5
0x02A6   13     770.5: CF
0x02B3   16     0: Boost/Cut
0x02C3    7  Band 6
0x02CA   12     1425: CF
0x02D6   16     0: Boost/Cut
0x02E6    7  Band 7
0x02ED   12     2645: CF
0x02F9   16     0: Boost/Cut
0x0309    7  Band 8
0x0310   12     4895: CF
0x031C   16     0: Boost/Cut
0x032C    7  Band 9
0x0333   12     9060: CF
0x033F   16     0: Boost/Cut
0x034F    8  Band 10
0x0357   13     13885: CF
0x0364   16     0: Boost/Cut
                                            <- EOF at 0x0374 = 884
```

Structural line count for a version-9, `double_params = 0`, `E = 1`, `NI = 7`,
`NR = NS = 0`, `NB` band file:

```
lines = 5            (magic, version, name, dbl flag, element count)
      + 6            (main params)
      + 1 + 7        (element 0)
      + 3            (app-dependent counts)
      + 7            (app-dependent integers)
      + 2            (EQ band count + on/off)
      + 3 * NB       (Band label + CF + Boost/Cut)
      = 31 + 3*NB    -> 61 lines for NB = 10
```

---

## 5. Working Python parser and its real output

The parser below was written against the C++ and **actually executed** against all 32
`.fac` files in the tree. It lives at
`/tmp/claude-1000/.../scratchpad/facparse.py` during this session; reproduce it verbatim
if you want to re-verify.

```python
#!/usr/bin/env python3
"""Reference parser for FxSound/DFX ".fac" (vals CLASS1) preset files."""
import sys, os, glob

MAIN_NAMES   = ["Fidelity", "Surround", "(unused)", "Ambience", "DynamicBoost", "BassBoost"]
APPINT_NAMES = ["fidelity_on", "surround_on", "ambience_on",
                "dynamic_boost_on", "bass_boost_on", "headphone_on", "music_mode"]

class FacError(Exception): pass

def _scan_int(line):                      # mimic swscanf(L"%d")
    s = line.lstrip(" \t"); i = 0
    if i < len(s) and s[i] in "+-": i += 1
    j = i
    while j < len(s) and s[j].isdigit(): j += 1
    if j == i: raise FacError("no integer in %r" % line)
    return int(s[:j])

def _scan_float(line):                    # mimic swscanf(L"%g")
    s = line.lstrip(" \t"); i = 0
    if i < len(s) and s[i] in "+-": i += 1
    seen = False
    while i < len(s) and s[i].isdigit(): i += 1; seen = True
    if i < len(s) and s[i] == ".":
        i += 1
        while i < len(s) and s[i].isdigit(): i += 1; seen = True
    if seen and i < len(s) and s[i] in "eE":
        j = i + 1
        if j < len(s) and s[j] in "+-": j += 1
        k = j
        while k < len(s) and s[k].isdigit(): k += 1
        if k > j: i = k
    if not seen: raise FacError("no float in %r" % line)
    return float(s[:i])

def parse_fac(path):
    raw   = open(path, "rb").read()
    lines = raw.decode("utf-8", errors="replace").split("\n")
    lines = [l[:-1] if l.endswith("\r") else l for l in lines]
    if lines and lines[-1] == "": lines.pop()
    pos = 0
    def nxt():
        nonlocal pos
        if pos >= len(lines): raise FacError("unexpected EOF at line %d" % pos)
        l = lines[pos]; pos += 1; return l

    out = {"path": path, "size": len(raw)}
    magic = nxt(); out["magic_line"] = magic
    if (magic.split()[0] if magic.split() else "") != "CLASS1":
        raise FacError("not a CLASS1 preset")
    out["version"] = _scan_float(nxt())
    out["name"]    = nxt()
    out["double_params"] = _scan_int(nxt()) if out["version"] > 1.0 else 0
    dbl = bool(out["double_params"])
    out["total_num_elements"] = _scan_int(nxt())

    main, main2 = [], []
    for _ in range(6):                                   # VALS_NUM_MAIN_PARAMS
        main.append(_scan_int(nxt()))
        if dbl: main2.append(_scan_int(nxt()))
    out["main_params"] = main
    if dbl: out["main_params_2"] = main2

    elements = []
    for ei in range(out["total_num_elements"]):
        if _scan_int(nxt()) != ei: raise FacError("element index mismatch")
        params, params2 = [], []
        for _ in range(7):                               # VALS_NUM_ELEMENT_PARAMS
            params.append(_scan_int(nxt()))
            if dbl: params2.append(_scan_int(nxt()))
        e = {"params": params}
        if dbl: e["params_2"] = params2
        elements.append(e)
    out["elements"] = elements

    n_int, n_real, n_str = _scan_int(nxt()), _scan_int(nxt()), _scan_int(nxt())
    out["num_app_ints"], out["num_app_reals"], out["num_app_strings"] = n_int, n_real, n_str
    out["app_ints"]  = [_scan_int(nxt())   for _ in range(n_int)]
    out["app_reals"] = [_scan_float(nxt()) for _ in range(n_real)]
    strs = []
    for _ in range(n_str):
        nxt()                                            # "String[i]:" label, skipped
        strs.append(nxt())
    out["app_strings"] = strs

    out["eq"] = None
    if out["version"] >= 9:
        n_bands = _scan_int(nxt()); eq_on = _scan_int(nxt())
        bands = []
        for b in range(1, n_bands + 1):
            nxt()                                        # "Band N" label, skipped
            cf = _scan_float(nxt()); bc = _scan_float(nxt())
            bands.append({"band": b, "cf_hz": cf, "boost_db": bc})
        out["eq"] = {"num_bands": n_bands, "eq_on": eq_on, "bands": bands}
    out["trailing_lines"] = lines[pos:]

    d = {}
    for k, idx in (("fidelity",0),("surround",1),("ambience",3),
                   ("dynamic_boost",4),("bass_boost",5)):
        d[k + "_midi"] = main[idx]
        d[k] = main[idx] / 127.0          # qntIToRCalc, LINEAR, 0..127 -> 0.0..1.0
    for i, nm in enumerate(APPINT_NAMES):
        d[nm] = out["app_ints"][i] if i < len(out["app_ints"]) else None
    out["decoded"] = d
    return out
```

### 5.1 Real output (verbatim, 6 of the 32 files)

```
$ python3 facparse.py "bin/BonusPresets/70's.fac" "bin/BonusPresets/Jazz.fac" \
                      "bin/BonusPresets/Metal.fac" "bin/BonusPresets/Quizal Star.fac" \
                      "Installer/Resources/Factsoft/1.fac" \
                      "Installer/Resources/Factsoft/Default.fac"

== 70's.fac  (881 bytes)
   magic='CLASS1 : Effect Type' ver=9 name="70's" dbl=0 elems=1 ints/reals/strs=7/0/0 trailing=0
   main(midi)=[76, 0, 0, 89, 38, 76]  elem0=[0, 0, 0, 0, 0, 0, 0]
   fid= 76(0.5984)+ sur=  0(0.0000)+ amb= 89(0.7008)+ dyn= 38(0.2992)+ bass= 76(0.5984)+ hp=0 mode=2
   eq_on=1 bands=10 : 62.5/0 110/2 200/1 295/3 650/1 1200/1 2150/0 4550/2 6300/-1 16000/-1

== Jazz.fac  (888 bytes)
   magic='CLASS1 : Effect Type' ver=9 name='Jazz' dbl=0 elems=1 ints/reals/strs=7/0/0 trailing=0
   main(midi)=[50, 20, 0, 0, 60, 60]  elem0=[0, 0, 0, 0, 0, 0, 0]
   fid= 50(0.3937)+ sur= 20(0.1575)+ amb=  0(0.0000)- dyn= 60(0.4724)+ bass= 60(0.4724)+ hp=0 mode=2
   eq_on=1 bands=10 : 62.5/4.72441 115/0 250/1 450/2 630/0 1250/-1 2700/0 5300/-1 7500/-2 13000/0

== Metal.fac  (907 bytes)
   magic='CLASS1 : Effect Type' ver=9 name='Metal' dbl=0 elems=1 ints/reals/strs=7/0/0 trailing=0
   main(midi)=[25, 20, 0, 38, 38, 25]  elem0=[0, 0, 0, 0, 0, 0, 0]
   fid= 25(0.1969)+ sur= 20(0.1575)+ amb= 38(0.2992)- dyn= 38(0.2992)+ bass= 25(0.1969)+ hp=0 mode=2
   eq_on=1 bands=10 : 62.5/1.9685 109.43/1 266.54/1 293/0 738.37/0 1355.22/0 2567.15/0 4719.84/0 8573.18/0 16000/1

== Quizal Star.fac  (962 bytes)
   magic='CLASS1 : Effect Type' ver=9 name='Quizal Star' dbl=0 elems=1 ints/reals/strs=7/0/0 trailing=0
   main(midi)=[0, 0, 0, 0, 127, 64]  elem0=[0, 0, 0, 0, 0, 0, 0]
   fid=  0(0.0000)+ sur=  0(0.0000)+ amb=  0(0.0000)+ dyn=127(1.0000)+ bass= 64(0.5039)+ hp=0 mode=2
   eq_on=1 bands=10 : 62.5/5.03937 157/6 292/-3 540/6 541/6 1010/0 2661.17/-3 4894.5/6 8951.04/6 11768/0

== 1.fac  (884 bytes)
   magic='CLASS1 : Effect Type' ver=9 name='General' dbl=0 elems=1 ints/reals/strs=7/0/0 trailing=0
   main(midi)=[50, 20, 0, 0, 60, 60]  elem0=[0, 0, 0, 0, 0, 0, 0]
   fid= 50(0.3937)+ sur= 20(0.1575)+ amb=  0(0.0000)- dyn= 60(0.4724)+ bass= 60(0.4724)+ hp=0 mode=2
   eq_on=1 bands=10 : 62.5/0 115/0 250/1 450/2 630/0 1250/-1 2700/0 5300/-1 7500/-2 13000/0

== Default.fac  (884 bytes)
   magic='CLASS1 : Effect Type' ver=9 name='Default' dbl=0 elems=1 ints/reals/strs=7/0/0 trailing=0
   main(midi)=[0, 0, 0, 0, 0, 0]  elem0=[0, 0, 0, 0, 0, 0, 0]
   fid=  0(0.0000)+ sur=  0(0.0000)+ amb=  0(0.0000)+ dyn=  0(0.0000)+ bass=  0(0.0000)+ hp=0 mode=2
   eq_on=1 bands=10 : 62.5/0 121.5/0 225/0 416.5/0 770.5/0 1425/0 2645/0 4895/0 9060/0 13885/0
```

(`+` / `-` after each value is the stored on-flag; `CF/boost` pairs are `Hz/dB`.)

**All 32 `.fac` files in the tree parse without error** under this parser:
19 in `bin/BonusPresets/`, 13 in `Installer/Resources/Factsoft/`
(the `bin/{x64,x86,arm64}/Factsoft/` copies are byte-identical duplicates of 12 of those 13).

### 5.2 Round-trip proof - writing `.fac` back out

A writer that reimplements `valsSave()`'s exact `printf` formats (`%d`, `%g`, the three
leading spaces on `Param`/`CF`/`Boost/Cut` lines) plus per-file newline-style and
final-newline detection was run against all 32 files:

```
$ python3 facwrite.py "bin/BonusPresets/*.fac" "Installer/Resources/Factsoft/*.fac"
BYTE-IDENTICAL  70's.fac (881 bytes)
BYTE-IDENTICAL  80's.fac (881 bytes)
BYTE-IDENTICAL  Alternative Rock.fac (900 bytes)
BYTE-IDENTICAL  Bass (Quizal).fac (951 bytes)
BYTE-IDENTICAL  Bass - ambience (Quizal).fac (963 bytes)
BYTE-IDENTICAL  Classic Rock.fac (889 bytes)
BYTE-IDENTICAL  Classical.fac (893 bytes)
BYTE-IDENTICAL  Conotating Life (Quizal).fac (977 bytes)
BYTE-IDENTICAL  Jazz.fac (888 bytes)
BYTE-IDENTICAL  Metal.fac (907 bytes)
BYTE-IDENTICAL  Modern Country.fac (891 bytes)
BYTE-IDENTICAL  Modern Rock.fac (897 bytes)
BYTE-IDENTICAL  Panserotaliya (Quizal).fac (965 bytes)
BYTE-IDENTICAL  Pop.fac (890 bytes)
BYTE-IDENTICAL  Quizal Star.fac (962 bytes)
BYTE-IDENTICAL  R&B.fac (905 bytes)
BYTE-IDENTICAL  Trap.fac (881 bytes)
BYTE-IDENTICAL  Underworld Life (Quizal).fac (973 bytes)
BYTE-IDENTICAL  Yoznogarda dy flybe - Life (Quizal).fac (981 bytes)
BYTE-IDENTICAL  1.fac (884 bytes)
BYTE-IDENTICAL  10.fac (902 bytes)
BYTE-IDENTICAL  11.fac (900 bytes)
BYTE-IDENTICAL  12.fac (894 bytes)
DIFFERS         2.fac  orig=898 new=882
DIFFERS         3.fac  orig=904 new=902
BYTE-IDENTICAL  4.fac (902 bytes)
DIFFERS         5.fac  orig=905 new=901
BYTE-IDENTICAL  6.fac (921 bytes)
BYTE-IDENTICAL  7.fac (923 bytes)
DIFFERS         8.fac  orig=910 new=904
DIFFERS         9.fac  orig=917 new=915
BYTE-IDENTICAL  Default.fac (884 bytes)

27 byte-identical, 5 differing, 32 total
```

**The 5 failures are not parser bugs - those files were never written by `valsSave()`.**
Every difference is of exactly this shape:

```
   line  36: orig=b'   110.0: CF'   new=b'   110: CF'      (2.fac)
   line  54: orig=b'   5250.0: CF'  new=b'   5250: CF'     (3.fac)
   line  42: orig=b'   444.0: CF'   new=b'   444: CF'      (5.fac)
   line  36: orig=b'   98.0: CF'    new=b'   98: CF'       (8.fac)
   line  54: orig=b'   5350.0: CF'  new=b'   5350: CF'     (9.fac)
```

C's `%g` **cannot** emit `110.0` - it strips trailing zeros and would print `110`. These
five factory presets therefore contain **hand-edited** frequency values. That is a fact
about the shipped data, not about the format.

**Conclusion: byte-identical write-back is feasible**, provided the port preserves three
things that are not part of the logical model:

1. the file's newline convention (LF vs CRLF) - CRLF appears in 7 shipped files;
2. whether the final line is newline-terminated (`Installer/Resources/Factsoft/1.fac` is not);
3. for files not authored by `valsSave()`, the **raw numeric token text** of any value that
   `%g` cannot reproduce.

The pragmatic design: keep a `raw: Option<String>` beside every float, populated on parse,
emitted verbatim on write when the value is unchanged, and replaced by a fresh `%g` render
when the user edits it. That gives byte-identical rewrites for untouched presets and
`valsSave()`-compatible output for edited ones.

---

## 6. Preset lifecycle in the shipping app (what the port must reproduce)

### 6.1 Directories (Windows) and their Linux equivalents

| Role | Windows path | Source | Linux equivalent (recommended) |
|---|---|---|---|
| Factory presets | `<cwd>/Factsoft/*.fac` | `FxController.cpp:844-846` | `$XDG_DATA_DIRS/fxsound/presets/factory/` then `/usr/share/fxsound/presets/factory/` |
| User presets | `%APPDATA%\FxSound\Presets\*.fac` | `FxController.cpp:856-858, 740` | `$XDG_DATA_HOME/fxsound/presets/` (`~/.local/share/fxsound/presets/`) |
| Auto-save (dirty state) | `%APPDATA%\FxSound\AutoSave\<name>.fac` | `FxController.cpp:803-812` | `$XDG_STATE_HOME/fxsound/autosave/` (`~/.local/state/fxsound/autosave/`) |
| Export target | `%USERPROFILE%\Documents\FxSound\Presets\Export\` | `FxController.cpp:1386` | `$XDG_DOCUMENTS_DIR/FxSound/Presets/Export/` via `xdg-user-dirs`, falling back to `~/Documents/...` |
| Bonus presets (installer payload) | `bin/BonusPresets/*.fac` (+ `BonusPresets.zip`) | repo | ship as data, or offer an in-app "install bonus presets" action |

`Factsoft` is resolved **relative to the process working directory**, which is a Windows
install-dir assumption. On Linux resolve it from an env override
(`FXSOUND_FACTORY_PRESET_DIR`) first, then XDG data dirs, then a compiled-in prefix - never
from `std::env::current_dir()`.

Note the Windows-only bit: `prelstConstructFullpath()` and `valsSave()` both hardcode
`\\` as the separator (`Prelst.cpp:160-162`, `Valsfile.cpp:69`). Use `std::path::Path::join`.

### 6.2 Enumeration

`FxController::initPresets()` (`FxController.cpp:841-876`):

1. glob `Factsoft/*.fac` (non-recursive, `File::findFiles`), call `getPresetInfo()` on each,
   **skip any file whose decoded name is empty**, push as `PresetType::AppPreset`;
2. glob `%APPDATA%\FxSound\Presets\*.fac` likewise, push as `PresetType::UserPreset`;
3. for every preset, if `AutoSave/<name>.fac` exists, mark `modified = true`.

**Ordering is filesystem-glob order, not sorted** - JUCE's `findChildFiles` returns
directory order. The port should sort deterministically (factory first in the numeric
order `1..12` then by name; user presets by name) and say so in the UI, because the
Windows ordering is effectively arbitrary.

The GUI `Preset` record is `{ name: String, path: String, type: PresetType, modified: bool }`
(`fxsound/Source/GUI/FxModel.h:34-40`). `PresetType { AppPreset = 1, UserPreset = 2 }`
(`FxModel.h:32`).

### 6.3 Select / load

`FxController::setPreset(index, notify)` (`FxController.cpp:1048-1097`):

1. if switching away from a modified preset -> `autoSavePreset(current)` first (`:1061-1065`);
2. if `AutoSave/<name>.fac` exists -> load **that** and mark modified (`:1070-1075`);
   else load the preset's own path and mark clean (`:1076-1080`);
3. persist `settings_["preset"] = name` (`:1082`);
4. re-scale the 5 effect values by `*10` through `setEffectValue()` (`:1085-1089`)
   - which also re-derives each effect's on/off from `value != 0` (§3.4);
5. re-apply every band's frequency and boost through the public setters (`:1091-1096`).

> Step 5 has an off-by-one against the rest of the codebase: it loops `b = 0 .. num_bands-1`
> and calls `setEqBandFrequency(b, ...)`, while `GraphicEqSetBandFreq()` requires
> `i_band_num >= 1` and rejects `0` (`GraphicEqSet.cpp:550-551`). So band `0` is a no-op
> and the top band is never refreshed. **Do not reproduce this; use 1-based band numbers
> consistently in the Rust port.**

### 6.4 Save / rename / delete / reset

* **Save over** (`FxController.cpp:1213-1222`): `savePreset(preset.name)` into the user
  presets dir, then delete the auto-save file, then clear the modified flag.
  Note this writes into `%APPDATA%\FxSound\Presets` even for a factory preset, effectively
  shadowing it with a user copy of the same name.
* **Save as new** (`:1223-1241`): same write, then `initPresets()`, then `setPreset(name)`.
  Toast `"Reached the limit on new presets."` when `getUserPresetCount() == max_user_presets_`.
  `max_user_presets_` comes from settings, validated to `10..=120`, default **120**
  (`FxController.cpp:194-198`).
* **Rename** (`:1244-1276`): user presets only. Writes a **new** file under the new name,
  then deletes the old path with `SHFileOperation(FO_DELETE)`. It is a copy-then-delete, not
  a rename - so a crash in between leaves both files.
* **Delete** (`:1278-1332`): user presets only; `SHFileOperation(FO_DELETE)` (sends to the
  Recycle Bin semantics of `FO_DELETE` with `FOF_NOCONFIRMATION|FOF_NOERRORUI|FOF_SILENT`).
* **Reset to factory** (`:1334-1382`): resets num-bands/volume-levelling/balance/filter-Q/
  master-gain to the `DEFAULT_*` constants, deletes every auto-save, deletes every user
  preset file, re-enumerates.
* **Export** (`:1384-1417`): per preset, `exportPreset(src, name, dest_dir)` which
  *reads then re-writes* the file through `valsRead`/`valsSave`
  (`dsp/DfxDspPreset.cpp:117-135`) - so an export **normalises** the file and will lose the
  hand-edited `110.0` style tokens described in §5.2. Overwrite is confirmed per file.
* **Import** (`:1419-1458`): `getPresetInfo()` for the name, reject if
  `isPresetNameValid()` is false (case-insensitive duplicate check against every loaded
  preset, `FxModel.cpp:142-153`), else **plain file copy** into the user presets dir as
  `<decoded name>.fac` - note the destination filename comes from the *decoded preset name*,
  not the source filename.

### 6.5 Filename vs. preset name

They are independent. The file's basename is only used to find the file; the displayed name
is the line-3 comment. `savePreset()` and the import path make them agree
(`dsp/DfxDspPreset.cpp:106`, `FxController.cpp:1435`), but a hand-authored file may disagree
and the app will happily show the comment. `Installer/Resources/Factsoft/1.fac` is named
`General`, `2.fac` is `Music`, etc.

**Name characters that will bite on Linux:** `/` is legal inside a preset name but not in a
filename, and `R&B.fac` already shows `&` in use. Sanitise the *filename* (replace
`/` and NUL), never the stored name.

---

## 7. Every bundled preset, decoded

`fid/sur/amb/dyn/bass` are the raw MIDI integers from `Main 0/1/3/4/5`
(divide by 127 for the 0..1 DSP value, multiply that by 10 for the slider).
`on-flags` are `Integer[0..4]` in the order Fidelity/Surround/Ambience/DynBoost/Bass.
`hp` = `Integer[5]` (headphone), `mode` = `Integer[6]` (music mode).

#### Factory presets - `Installer/Resources/Factsoft/*.fac` (also `bin/{x64,x86,arm64}/Factsoft/`)

| file | preset name | fid | sur | amb | dyn | bass | on-flags F/S/A/D/B | hp | mode | eq_on |
|---|---|---|---|---|---|---|---|---|---|---|
| `1.fac` | General | 50 | 20 | 0 | 60 | 60 | 1/1/0/1/1 | 0 | 2 | 1 |
| `10.fac` | Movies | 60 | 50 | 0 | 85 | 35 | 1/1/1/1/1 | 0 | 2 | 1 |
| `11.fac` | TV | 50 | 50 | 20 | 60 | 45 | 1/1/1/1/1 | 0 | 2 | 1 |
| `12.fac` | Transcription | 100 | 0 | 0 | 115 | 75 | 1/0/0/1/1 | 0 | 2 | 1 |
| `2.fac` | Music | 50 | 35 | 35 | 20 | 60 | 1/1/1/1/1 | 0 | 2 | 1 |
| `3.fac` | Voice | 72 | 0 | 0 | 95 | 0 | 1/0/0/1/0 | 0 | 2 | 1 |
| `4.fac` | Volume Boost | 32 | 20 | 0 | 103 | 35 | 1/1/1/1/1 | 0 | 2 | 1 |
| `5.fac` | Gaming | 35 | 0 | 0 | 85 | 35 | 1/0/0/1/1 | 0 | 2 | 1 |
| `6.fac` | Classic Processing | 60 | 35 | 60 | 60 | 70 | 1/1/1/1/1 | 0 | 2 | 1 |
| `7.fac` | Light Processing | 25 | 0 | 35 | 5 | 20 | 1/0/1/1/1 | 0 | 2 | 1 |
| `8.fac` | Bass Boost | 30 | 35 | 35 | 20 | 75 | 1/1/1/1/1 | 0 | 2 | 1 |
| `9.fac` | Streaming Video | 35 | 35 | 0 | 54 | 35 | 1/1/0/1/1 | 0 | 2 | 1 |
| `Default.fac` | Default | 0 | 0 | 0 | 0 | 0 | 1/1/1/1/1 | 0 | 2 | 1 |

| file | B1 | B2 | B3 | B4 | B5 | B6 | B7 | B8 | B9 | B10 |
|---|---|---|---|---|---|---|---|---|---|---|
| `1.fac` | 62.5 Hz<br>0 dB | 115 Hz<br>0 dB | 250 Hz<br>1 dB | 450 Hz<br>2 dB | 630 Hz<br>0 dB | 1250 Hz<br>-1 dB | 2700 Hz<br>0 dB | 5300 Hz<br>-1 dB | 7500 Hz<br>-2 dB | 13000 Hz<br>0 dB |
| `10.fac` | 62.5 Hz<br>0 dB | 115.734 Hz<br>0 dB | 250 Hz<br>2 dB | 396.85 Hz<br>0 dB | 734.867 Hz<br>2 dB | 1360.79 Hz<br>2 dB | 2519.84 Hz<br>1 dB | 5350 Hz<br>-1 dB | 8640.48 Hz<br>0 dB | 13800 Hz<br>2 dB |
| `11.fac` | 62.5 Hz<br>0 dB | 115.734 Hz<br>0 dB | 250 Hz<br>1 dB | 396.85 Hz<br>0 dB | 734.867 Hz<br>1 dB | 1360.79 Hz<br>0 dB | 2519.84 Hz<br>1 dB | 5350 Hz<br>-1 dB | 8640.48 Hz<br>-1 dB | 13800 Hz<br>2 dB |
| `12.fac` | 62.5 Hz<br>0 dB | 86 Hz<br>-12 dB | 250 Hz<br>7 dB | 293 Hz<br>2 dB | 615 Hz<br>-1 dB | 1320 Hz<br>7 dB | 3430 Hz<br>0 dB | 4630 Hz<br>10 dB | 6360 Hz<br>3 dB | 11770 Hz<br>-12 dB |
| `2.fac` | 62.5 Hz<br>0 dB | 110 Hz<br>2 dB | 250 Hz<br>2 dB | 370 Hz<br>1 dB | 650 Hz<br>0 dB | 1200 Hz<br>0 dB | 2130 Hz<br>0 dB | 4550 Hz<br>-1 dB | 6850 Hz<br>0 dB | 16000 Hz<br>2 dB |
| `3.fac` | 62.5 Hz<br>0 dB | 115.734 Hz<br>-4 dB | 214.311 Hz<br>-2 dB | 396.85 Hz<br>2 dB | 734.867 Hz<br>4 dB | 1360.79 Hz<br>5 dB | 3430.8 Hz<br>3 dB | 5250 Hz<br>3 dB | 6300 Hz<br>5 dB | 11770 Hz<br>-11 dB |
| `4.fac` | 62.5 Hz<br>0 dB | 101 Hz<br>3 dB | 240 Hz<br>2 dB | 396.85 Hz<br>2 dB | 734.867 Hz<br>0 dB | 1360.79 Hz<br>0 dB | 2519.84 Hz<br>1 dB | 4670 Hz<br>1 dB | 11760 Hz<br>2 dB | 16000 Hz<br>2 dB |
| `5.fac` | 62.5 Hz<br>0 dB | 128.75 Hz<br>0 dB | 238.311 Hz<br>2 dB | 444 Hz<br>2 dB | 805 Hz<br>2 dB | 1360.79 Hz<br>0 dB | 2519.84 Hz<br>-1 dB | 4400.12 Hz<br>-1 dB | 7930.48 Hz<br>2 dB | 12570 Hz<br>2 dB |
| `6.fac` | 62.5 Hz<br>0 dB | 115.734 Hz<br>0 dB | 214.311 Hz<br>0 dB | 396.85 Hz<br>0 dB | 734.867 Hz<br>0 dB | 1360.79 Hz<br>0 dB | 2519.84 Hz<br>0 dB | 4666.12 Hz<br>0 dB | 8640.48 Hz<br>0 dB | 13500 Hz<br>0 dB |
| `7.fac` | 62.5 Hz<br>0 dB | 115.734 Hz<br>-1 dB | 214.311 Hz<br>1 dB | 396.85 Hz<br>1 dB | 734.867 Hz<br>-1 dB | 1360.79 Hz<br>-1 dB | 2519.84 Hz<br>-2 dB | 4666.12 Hz<br>-1 dB | 8640.48 Hz<br>-1 dB | 13600 Hz<br>1 dB |
| `8.fac` | 62.5 Hz<br>0 dB | 98 Hz<br>3 dB | 158.3 Hz<br>3 dB | 345 Hz<br>2 dB | 541.867 Hz<br>1 dB | 1170 Hz<br>-1 dB | 2519.84 Hz<br>-1 dB | 4666.12 Hz<br>-1 dB | 8640.48 Hz<br>-1 dB | 14650 Hz<br>0 dB |
| `9.fac` | 62.5 Hz<br>0 dB | 115.734 Hz<br>0 dB | 214.311 Hz<br>0 dB | 396.85 Hz<br>0 dB | 734.867 Hz<br>1 dB | 1360.79 Hz<br>1 dB | 2519.84 Hz<br>1 dB | 5350 Hz<br>-1 dB | 8640.48 Hz<br>0 dB | 13800 Hz<br>2 dB |
| `Default.fac` | 62.5 Hz<br>0 dB | 121.5 Hz<br>0 dB | 225 Hz<br>0 dB | 416.5 Hz<br>0 dB | 770.5 Hz<br>0 dB | 1425 Hz<br>0 dB | 2645 Hz<br>0 dB | 4895 Hz<br>0 dB | 9060 Hz<br>0 dB | 13885 Hz<br>0 dB |

#### Bonus presets - `bin/BonusPresets/*.fac`

| file | preset name | fid | sur | amb | dyn | bass | on-flags F/S/A/D/B | hp | mode | eq_on |
|---|---|---|---|---|---|---|---|---|---|---|
| `70's.fac` | 70's | 76 | 0 | 89 | 38 | 76 | 1/1/1/1/1 | 0 | 2 | 1 |
| `80's.fac` | 80's | 76 | 0 | 89 | 38 | 76 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Alternative Rock.fac` | Alternative Rock | 50 | 20 | 0 | 60 | 60 | 1/1/0/1/1 | 0 | 2 | 1 |
| `Bass (Quizal).fac` | Bass (Quizal) | 0 | 0 | 0 | 0 | 127 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Bass - ambience (Quizal).fac` | Bass - ambience (Quizal) | 0 | 64 | 0 | 0 | 127 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Classic Rock.fac` | Classic Rock | 76 | 0 | 89 | 38 | 76 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Classical.fac` | Classical | 50 | 20 | 0 | 60 | 60 | 1/1/0/1/1 | 0 | 2 | 1 |
| `Conotating Life (Quizal).fac` | Conotating Life (Quizal) | 25 | 38 | 0 | 127 | 64 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Jazz.fac` | Jazz | 50 | 20 | 0 | 60 | 60 | 1/1/0/1/1 | 0 | 2 | 1 |
| `Metal.fac` | Metal | 25 | 20 | 38 | 38 | 25 | 1/1/0/1/1 | 0 | 2 | 1 |
| `Modern Country.fac` | Modern Country | 76 | 0 | 89 | 38 | 76 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Modern Rock.fac` | Modern Rock | 38 | 0 | 13 | 89 | 25 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Panserotaliya (Quizal).fac` | Panserotaliya (Quizal) | 0 | 0 | 0 | 0 | 127 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Pop.fac` | Pop | 38 | 0 | 13 | 89 | 25 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Quizal Star.fac` | Quizal Star | 0 | 0 | 0 | 127 | 64 | 1/1/1/1/1 | 0 | 2 | 1 |
| `R&B.fac` | R&B | 25 | 20 | 38 | 38 | 25 | 1/1/0/1/1 | 0 | 2 | 1 |
| `Trap.fac` | Trap | 76 | 0 | 89 | 38 | 76 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Underworld Life (Quizal).fac` | Underworld Life (Quizal) | 25 | 38 | 0 | 127 | 64 | 1/1/1/1/1 | 0 | 2 | 1 |
| `Yoznogarda dy flybe - Life (Quizal).fac` | Yoznogarda dy flybe - Life (Quizal) | 25 | 64 | 0 | 127 | 127 | 1/1/1/1/1 | 0 | 2 | 1 |

| file | B1 | B2 | B3 | B4 | B5 | B6 | B7 | B8 | B9 | B10 |
|---|---|---|---|---|---|---|---|---|---|---|
| `70's.fac` | 62.5 Hz<br>0 dB | 110 Hz<br>2 dB | 200 Hz<br>1 dB | 295 Hz<br>3 dB | 650 Hz<br>1 dB | 1200 Hz<br>1 dB | 2150 Hz<br>0 dB | 4550 Hz<br>2 dB | 6300 Hz<br>-1 dB | 16000 Hz<br>-1 dB |
| `80's.fac` | 62.5 Hz<br>0 dB | 110 Hz<br>2 dB | 200 Hz<br>1 dB | 295 Hz<br>3 dB | 650 Hz<br>1 dB | 1200 Hz<br>1 dB | 2120 Hz<br>0 dB | 4550 Hz<br>2 dB | 6300 Hz<br>-1 dB | 16000 Hz<br>-1 dB |
| `Alternative Rock.fac` | 62.5 Hz<br>4.72441 dB | 115 Hz<br>0 dB | 250 Hz<br>1 dB | 450 Hz<br>2 dB | 630 Hz<br>0 dB | 1250 Hz<br>-1 dB | 2700 Hz<br>0 dB | 5300 Hz<br>-1 dB | 7500 Hz<br>-2 dB | 13000 Hz<br>0 dB |
| `Bass (Quizal).fac` | 62.5 Hz<br>10 dB | 157 Hz<br>6 dB | 292 Hz<br>-3 dB | 540 Hz<br>6 dB | 1000 Hz<br>-6 dB | 1010 Hz<br>0 dB | 1862 Hz<br>0 dB | 3439 Hz<br>0 dB | 6360 Hz<br>0 dB | 11768 Hz<br>0 dB |
| `Bass - ambience (Quizal).fac` | 62.5 Hz<br>10 dB | 157 Hz<br>6 dB | 292 Hz<br>-3 dB | 540 Hz<br>6 dB | 1000 Hz<br>-6 dB | 1010 Hz<br>0 dB | 1862 Hz<br>0 dB | 3439 Hz<br>0 dB | 6360 Hz<br>0 dB | 11768 Hz<br>0 dB |
| `Classic Rock.fac` | 62.5 Hz<br>0 dB | 110 Hz<br>2 dB | 200 Hz<br>1 dB | 295 Hz<br>3 dB | 650 Hz<br>1 dB | 1200 Hz<br>1 dB | 2130 Hz<br>0 dB | 4550 Hz<br>2 dB | 6360 Hz<br>-1 dB | 16000 Hz<br>-1 dB |
| `Classical.fac` | 62.5 Hz<br>4.72441 dB | 115 Hz<br>0 dB | 250 Hz<br>1 dB | 450 Hz<br>2 dB | 630 Hz<br>0 dB | 1250 Hz<br>-1 dB | 2700 Hz<br>0 dB | 5300 Hz<br>-1 dB | 7500 Hz<br>-2 dB | 13000 Hz<br>0 dB |
| `Conotating Life (Quizal).fac` | 62.5 Hz<br>5.03937 dB | 157 Hz<br>6 dB | 292 Hz<br>-3 dB | 540 Hz<br>6 dB | 541 Hz<br>6 dB | 1010 Hz<br>0 dB | 2661.17 Hz<br>-3 dB | 4894.5 Hz<br>6 dB | 8951.04 Hz<br>6 dB | 11768 Hz<br>0 dB |
| `Jazz.fac` | 62.5 Hz<br>4.72441 dB | 115 Hz<br>0 dB | 250 Hz<br>1 dB | 450 Hz<br>2 dB | 630 Hz<br>0 dB | 1250 Hz<br>-1 dB | 2700 Hz<br>0 dB | 5300 Hz<br>-1 dB | 7500 Hz<br>-2 dB | 13000 Hz<br>0 dB |
| `Metal.fac` | 62.5 Hz<br>1.9685 dB | 109.43 Hz<br>1 dB | 266.54 Hz<br>1 dB | 293 Hz<br>0 dB | 738.37 Hz<br>0 dB | 1355.22 Hz<br>0 dB | 2567.15 Hz<br>0 dB | 4719.84 Hz<br>0 dB | 8573.18 Hz<br>0 dB | 16000 Hz<br>1 dB |
| `Modern Country.fac` | 62.5 Hz<br>0 dB | 110 Hz<br>2 dB | 185 Hz<br>1 dB | 285 Hz<br>3 dB | 625 Hz<br>1 dB | 1200 Hz<br>1 dB | 2130 Hz<br>0 dB | 4550 Hz<br>2 dB | 6360 Hz<br>-1 dB | 16000 Hz<br>-1 dB |
| `Modern Rock.fac` | 62.5 Hz<br>1.9685 dB | 90 Hz<br>0 dB | 230 Hz<br>0 dB | 370 Hz<br>-1 dB | 650 Hz<br>-2 dB | 1200 Hz<br>-3 dB | 2125 Hz<br>-3 dB | 5300 Hz<br>-2 dB | 10000 Hz<br>-1 dB | 12000 Hz<br>0 dB |
| `Panserotaliya (Quizal).fac` | 62.5 Hz<br>10 dB | 157 Hz<br>6 dB | 292 Hz<br>-3 dB | 540 Hz<br>6 dB | 1000 Hz<br>-6 dB | 1010 Hz<br>0 dB | 1862 Hz<br>0 dB | 4894.5 Hz<br>6 dB | 8951.04 Hz<br>6 dB | 11768 Hz<br>0 dB |
| `Pop.fac` | 62.5 Hz<br>1.9685 dB | 110 Hz<br>0 dB | 230 Hz<br>0 dB | 370 Hz<br>-1 dB | 650 Hz<br>-2 dB | 1200 Hz<br>-3 dB | 2150 Hz<br>-3 dB | 5300 Hz<br>-2 dB | 10000 Hz<br>-1 dB | 12000 Hz<br>0 dB |
| `Quizal Star.fac` | 62.5 Hz<br>5.03937 dB | 157 Hz<br>6 dB | 292 Hz<br>-3 dB | 540 Hz<br>6 dB | 541 Hz<br>6 dB | 1010 Hz<br>0 dB | 2661.17 Hz<br>-3 dB | 4894.5 Hz<br>6 dB | 8951.04 Hz<br>6 dB | 11768 Hz<br>0 dB |
| `R&B.fac` | 62.5 Hz<br>1.9685 dB | 109.43 Hz<br>1 dB | 266.54 Hz<br>1 dB | 293 Hz<br>0 dB | 738.37 Hz<br>0 dB | 1355.22 Hz<br>0 dB | 2567.15 Hz<br>0 dB | 4719.84 Hz<br>0 dB | 8573.18 Hz<br>0 dB | 16000 Hz<br>1 dB |
| `Trap.fac` | 62.5 Hz<br>0 dB | 100 Hz<br>2 dB | 180 Hz<br>1 dB | 290 Hz<br>3 dB | 650 Hz<br>1 dB | 1200 Hz<br>1 dB | 2200 Hz<br>0 dB | 4550 Hz<br>2 dB | 6363 Hz<br>-1 dB | 16000 Hz<br>-1 dB |
| `Underworld Life (Quizal).fac` | 62.5 Hz<br>5.03937 dB | 157 Hz<br>6 dB | 292 Hz<br>-3 dB | 540 Hz<br>6 dB | 541 Hz<br>6 dB | 1010 Hz<br>0 dB | 1862 Hz<br>0 dB | 4894.5 Hz<br>6 dB | 8951.04 Hz<br>6 dB | 11768 Hz<br>0 dB |
| `Yoznogarda dy flybe - Life (Quizal).fac` | 62.5 Hz<br>10 dB | 157 Hz<br>6 dB | 292 Hz<br>-3 dB | 540 Hz<br>6 dB | 1000 Hz<br>-6 dB | 1010 Hz<br>0 dB | 1862 Hz<br>0 dB | 3439 Hz<br>-12 dB | 6360 Hz<br>-12 dB | 16000 Hz<br>0 dB |

### 7.1 Observations in the data worth knowing

* **Duplicate content.** `Jazz.fac`, `Classical.fac` and `Alternative Rock.fac` are
  logically identical (same mains, same EQ). `Metal.fac` and `R&B.fac` are identical.
  `70's.fac`, `80's.fac`, `Classic Rock.fac`, `Modern Country.fac` and `Trap.fac` differ
  only in one or two band centre frequencies. `Installer/Resources/Factsoft/1.fac`
  ("General") equals `Jazz.fac` except that band 1's boost is `0` instead of `4.72441`.
  Do not "deduplicate" them - users select by name.
* **Band 1 boost tracks bass-boost in the newer bonus presets.** In every preset authored
  with the newer editor, `band1_boost_db == (bass_midi / 127.0) * 10`:
  `60 -> 4.72441`, `25 -> 1.9685`, `64 -> 5.03937`, `127 -> 10`.
  This is a *property of how those presets were authored*, **not** something the code does:
  the only place that could have done it, `f_bass_boost_value` in
  `dsp/DfxDspEq.cpp:134`, is a dead variable. Reproduce the values, not the rule.
* **Band frequencies are not monotonic in two files.** `Quizal Star.fac`,
  `Conotating Life (Quizal).fac` and `Underworld Life (Quizal).fac` have bands 4 and 5 at
  `540` and `541` Hz - effectively two coincident filters. `Bass (Quizal).fac` &c. have
  bands 5/6 at `1000`/`1010`. The C++ neither validates nor sorts; the Rust port must not
  either, or those presets will change sound.
* **Preset frequencies override the live table.** `Metal.fac` / `R&B.fac` band 3 is
  `266.54 Hz` where the live 10-band table (§3.6) says `214.311 Hz`; because the band
  counts match, the preset's frequency wins (`dsp/DfxDspEq.cpp:230-241`). A port that
  assumes the fixed table and only loads gains will mis-render these presets.
* All 32 files: `version = 9`, `double_params = 0`, `total_num_elements = 1`,
  element params all `0`, `NI/NR/NS = 7/0/0`, `music_mode = 2`, `headphone_on = 0`,
  `eq_on = 1`, `num_bands = 10`. Sizes range 881..981 bytes.

---

## 8. Rust model and parser sketch

### 8.1 Types

```rust
// crates/fxsound-preset/src/lib.rs
use std::path::{Path, PathBuf};

pub const VALS_NUM_MAIN_PARAMS:    usize = 6;   // dsp/ptutil/include/vals.h:23
pub const VALS_NUM_ELEMENT_PARAMS: usize = 7;   // dsp/ptutil/include/vals.h:24
pub const VALS_MAX_NUM_ELEMENTS:   usize = 8;   // dsp/ptutil/include/vals.h:25
pub const NUM_APP_DEPEND_INTS:     usize = 7;   // dsp/DfxDspPreset.cpp:31
pub const VALS_FILE_VERSION:       f32   = 9.0; // dsp/DfxDspPreset.cpp:53
pub const MIDI_MIN: i32 = 0;                    // dsp/DfxDspPrivate.cpp:50
pub const MIDI_MAX: i32 = 127;                  // dsp/DfxDspPrivate.cpp:51
pub const EQ_MAX_BOOST_DB:  f32 = 20.0;         // GraphicEq.h:46
pub const EQ_MIN_BAND_HZ:   f32 = 10.0;         // GraphicEq.h:42
pub const EQ_MAX_BAND_HZ:   f32 = 21000.0;      // GraphicEq.h:43
pub const EQ_MAX_NUM_BANDS: usize = 32;         // sos.h:27 via u_GraphicEq.h:35
pub const MAX_PRESET_NAME_LEN: usize = 128;     // dsp/DfxDspPrivate.cpp:43
pub const VALS_LINE_LIMIT: usize = 128;         // Valsfile.cpp:38  (fgetws cap)

/// Main-param slot indices. NOTE the hole at 2.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum MainSlot { Fidelity = 0, Surround = 1, Unused = 2, Ambience = 3,
                    DynamicBoost = 4, BassBoost = 5 }

/// App-dependent integer slots. Contiguous 0..=6, DIFFERENT ordering from MainSlot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum AppIntSlot { Fidelity = 0, Surround = 1, Ambience = 2, DynamicBoost = 3,
                      BassBoost = 4, Headphone = 5, MusicMode = 6 }

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum MusicMode { Music1 = 1, Music2 = 2, Speech = 3 }   // dsp/DfxDspPreset.cpp:43-45

/// A float plus the exact token it came from, so untouched files rewrite byte-identically.
#[derive(Clone, Debug, PartialEq)]
pub struct Num { pub value: f32, pub raw: Option<Box<str>> }

impl Num {
    pub fn new(value: f32) -> Self { Self { value, raw: None } }
    /// C `printf("%g")`: 6 significant digits, trailing zeros stripped.
    pub fn render(&self) -> String {
        match &self.raw { Some(r) => r.to_string(), None => fmt_g(self.value) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EqBand { pub center_hz: Num, pub boost_db: Num }

#[derive(Clone, Debug, PartialEq)]
pub struct EqBlock { pub eq_on: bool, pub bands: Vec<EqBand> }   // 1..=32 bands

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Newline { Lf, CrLf }

#[derive(Clone, Debug, PartialEq)]
pub struct FacPreset {
    /// Line 1 verbatim ("CLASS1 : Effect Type" for everything shipped).
    pub effect_type_line: Box<str>,
    pub version:          Num,               // 9.0
    /// Line 3. Empty => the Windows app treats the file as nameless and skips it.
    pub name:             String,
    pub double_params:    bool,              // always false for DFX
    pub main_params:      [i32; VALS_NUM_MAIN_PARAMS],
    pub main_params_2:    Option<[i32; VALS_NUM_MAIN_PARAMS]>,
    pub elements:         Vec<[i32; VALS_NUM_ELEMENT_PARAMS]>,        // len == 1
    pub elements_2:       Option<Vec<[i32; VALS_NUM_ELEMENT_PARAMS]>>,
    pub app_ints:         Vec<i32>,          // len == 7
    pub app_reals:        Vec<Num>,          // len == 0
    pub app_strings:      Vec<String>,       // len == 0
    pub eq:               Option<EqBlock>,
    // --- byte-fidelity bookkeeping, not part of the logical model ---
    pub newline:          Newline,
    pub final_newline:    bool,
}

impl FacPreset {
    #[inline] pub fn midi(&self, s: MainSlot) -> i32 { self.main_params[s as usize] }
    /// qntIToRCalc, QNT_RESPONSE_LINEAR, 0..127 -> 0.0..1.0 (Qntitor.cpp:169-178)
    #[inline] pub fn value(&self, s: MainSlot) -> f32 { self.midi(s) as f32 / 127.0 }
    /// What the JUCE slider shows (FxAudioControls.cpp:113, FxController.cpp:1088)
    #[inline] pub fn slider(&self, s: MainSlot) -> f32 { self.value(s) * 10.0 }
    #[inline] pub fn flag(&self, s: AppIntSlot) -> bool {
        self.app_ints.get(s as usize).copied().unwrap_or(0) != 0
    }
    /// Observed behaviour: setEffectValue() overrides the stored flag (DfxDspPrivate.cpp:295-302)
    #[inline] pub fn effective_on(&self, s: MainSlot) -> bool { self.midi(s) != 0 }
}

/// qntRToICalc (Qntrtoi.cpp:96-97): truncating cast of (v*127 + 0.5) => round-half-up.
#[inline]
pub fn value_to_midi(v: f32) -> i32 { ((v * 127.0) + 0.5) as i32 }
```

`fmt_g` must reproduce C's `%g` exactly - Rust's `{}` for `f32` does **not**. Either bind
`libc::snprintf`, or implement it: `%g` = `%e` with precision 5 when the decimal exponent
is `< -4` or `>= 6`, else `%f` with precision `5 - exp`, then strip trailing zeros and a
trailing `.`. Unit-test it against the 32 shipped files; the `Num.raw` passthrough makes it
non-critical for read-modify-write, but a *newly saved* preset must match `valsSave()`.

### 8.2 Parser - hand-rolled, not `nom`

`nom` buys nothing here: the grammar is "split on newlines, then `%d`/`%g` the leading
token of each line by position". A hand-rolled line cursor is shorter, gives better error
messages, and makes the "must not resynchronise" property explicit.

```rust
#[derive(Debug, thiserror::Error)]
pub enum FacError {
    #[error("not a CLASS1 preset: first token was {0:?}")] BadMagic(String),
    #[error("unexpected end of file at line {0}")]          Eof(usize),
    #[error("line {0}: expected integer, got {1:?}")]       BadInt(usize, String),
    #[error("line {0}: expected number, got {1:?}")]        BadFloat(usize, String),
    #[error("line {0}: element index {got} != expected {want}")] ElementIndex{ line: usize, got: i32, want: usize },
    #[error("line {0}: {1} bands out of range 1..=32")]     BandCount(usize, i32),
    #[error("io")] Io(#[from] std::io::Error),
}

struct Cursor<'a> { lines: Vec<&'a str>, at: usize }

impl<'a> Cursor<'a> {
    fn next(&mut self) -> Result<&'a str, FacError> {
        let l = *self.lines.get(self.at).ok_or(FacError::Eof(self.at + 1))?;
        self.at += 1; Ok(l)
    }
    /// swscanf(L"%d"): skip blanks, optional sign, digits, stop anywhere else.
    fn int(&mut self) -> Result<i32, FacError> {
        let ln = self.at + 1; let s = self.next()?;
        scan_int(s).ok_or_else(|| FacError::BadInt(ln, s.to_owned()))
    }
    /// swscanf(L"%g"): skip blanks, C float literal prefix.
    fn num(&mut self) -> Result<Num, FacError> {
        let ln = self.at + 1; let s = self.next()?;
        scan_num(s).ok_or_else(|| FacError::BadFloat(ln, s.to_owned()))
    }
    fn skip(&mut self) -> Result<(), FacError> { self.next().map(|_| ()) }
}

pub fn parse(bytes: &[u8]) -> Result<FacPreset, FacError> {
    let newline = if bytes.windows(2).any(|w| w == b"\r\n") { Newline::CrLf } else { Newline::Lf };
    let final_newline = bytes.last() == Some(&b'\n');
    // valsRead() is lenient about encoding; be lossy rather than fail on a mangled name.
    let text = String::from_utf8_lossy(bytes);
    let mut lines: Vec<&str> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    if lines.last() == Some(&"") { lines.pop(); }
    let mut c = Cursor { lines, at: 0 };

    // 1. magic -- only the first whitespace token is checked (Valsfile.cpp:546-557)
    let effect_type_line = c.next()?;
    match effect_type_line.split_whitespace().next() {
        Some("CLASS1") => {}
        other => return Err(FacError::BadMagic(other.unwrap_or("").to_owned())),
    }

    let version = c.num()?;                                   // Valsfile.cpp:316
    let name    = c.next()?.to_owned();                       // Valsfile.cpp:320-341
    let double_params = if version.value > 1.0 { c.int()? != 0 } else { false }; // :350-358
    let n_elems = c.int()? as usize;                          // :364-365

    let mut main_params   = [0i32; VALS_NUM_MAIN_PARAMS];
    let mut main_params_2 = [0i32; VALS_NUM_MAIN_PARAMS];
    for i in 0..VALS_NUM_MAIN_PARAMS {                        // :368-378
        main_params[i] = c.int()?;
        if double_params { main_params_2[i] = c.int()?; }
    }

    let mut elements   = Vec::with_capacity(n_elems);
    let mut elements_2 = Vec::with_capacity(n_elems);
    for e in 0..n_elems {                                     // :381-402
        let line = c.at + 1;
        let got = c.int()?;
        if got as usize != e { return Err(FacError::ElementIndex { line, got, want: e }); }
        let (mut p, mut p2) = ([0i32; VALS_NUM_ELEMENT_PARAMS], [0i32; VALS_NUM_ELEMENT_PARAMS]);
        for k in 0..VALS_NUM_ELEMENT_PARAMS {
            p[k] = c.int()?;
            if double_params { p2[k] = c.int()?; }
        }
        elements.push(p);
        if double_params { elements_2.push(p2); }
    }

    let (n_int, n_real, n_str) = (c.int()? as usize, c.int()? as usize, c.int()? as usize); // :405-410
    let mut app_ints = Vec::with_capacity(n_int);
    for _ in 0..n_int  { app_ints.push(c.int()?); }           // :418-422
    let mut app_reals = Vec::with_capacity(n_real);
    for _ in 0..n_real { app_reals.push(c.num()?); }          // :425-429
    let mut app_strings = Vec::with_capacity(n_str);
    for _ in 0..n_str  { c.skip()?; app_strings.push(c.next()?.to_owned()); } // :432-450

    let eq = if version.value >= 9.0 {                        // :453
        let line = c.at + 1;
        let nb = c.int()?;                                    // :469-470
        if nb < 1 || nb as usize > EQ_MAX_NUM_BANDS { return Err(FacError::BandCount(line, nb)); }
        let eq_on = c.int()? != 0;                            // :473-474
        let mut bands = Vec::with_capacity(nb as usize);
        for _ in 0..nb {                                      // :481-499
            c.skip()?;                                        // "Band N"
            let center_hz = c.num()?;
            let boost_db  = c.num()?;
            bands.push(EqBand { center_hz, boost_db });
        }
        Some(EqBlock { eq_on, bands })
    } else { None };

    Ok(FacPreset { effect_type_line: effect_type_line.into(), version, name, double_params,
                   main_params, main_params_2: double_params.then_some(main_params_2),
                   elements, elements_2: double_params.then_some(elements_2),
                   app_ints, app_reals, app_strings, eq, newline, final_newline })
}
```

`byteorder` has no role: nothing in the format is a binary integer.

### 8.3 Writer

```rust
pub fn write(p: &FacPreset) -> Vec<u8> {
    let nl = match p.newline { Newline::Lf => "\n", Newline::CrLf => "\r\n" };
    let mut s = String::with_capacity(1024);
    macro_rules! line { ($($a:tt)*) => {{ s.push_str(&format!($($a)*)); s.push_str(nl); }} }

    line!("CLASS1 : Effect Type");                                   // Valsfile.cpp:76
    line!("{}: Version", p.version.render());                        // :78
    line!("{}", p.name);                                             // :80-90
    if p.version.value > 1.0 { line!("{}: Double Params Flag", p.double_params as i32); } // :93-96
    line!("{}: Total number of elements", p.elements.len());         // :98
    for (i, v) in p.main_params.iter().enumerate() {                 // :101-110
        line!("{v}: Main {i}");
        if let Some(m2) = &p.main_params_2 { line!("{}: Main_2 {i}", m2[i]); }
    }
    for (e, params) in p.elements.iter().enumerate() {               // :113-130
        line!("{e}: Element Number");
        for (k, v) in params.iter().enumerate() {
            line!("   {v}: Param {k}");
            if let Some(e2) = &p.elements_2 { line!("   {}: Param_2 {k}", e2[e][k]); }
        }
    }
    line!("{}: Number of Application Dependent Integers", p.app_ints.len());   // :133
    line!("{}: Number of Application Dependent Reals",    p.app_reals.len());  // :135
    line!("{}: Number of Application Dependent Strings",  p.app_strings.len());// :137
    for (i, v) in p.app_ints.iter().enumerate()   { line!("{v}: Integer[{i}]"); }        // :142
    for (i, v) in p.app_reals.iter().enumerate()  { line!("{}: Real[{i}]", v.render()); }// :148
    for (i, v) in p.app_strings.iter().enumerate(){ line!("String[{i}]:"); line!("{v}"); }// :154-158
    if let Some(eq) = &p.eq {                                        // :162-182
        line!("{}: Number of EQ Bands", eq.bands.len());
        line!("{}: On/Off Flag", eq.eq_on as i32);
        for (i, b) in eq.bands.iter().enumerate() {
            line!("Band {}", i + 1);                                 // 1-BASED on the wire
            line!("   {}: CF",        b.center_hz.render());
            line!("   {}: Boost/Cut", b.boost_db.render());
        }
    }
    let mut out = s.into_bytes();
    if !p.final_newline { out.truncate(out.len() - nl.len()); }
    out
}
```

**A newly authored preset must use `Newline::Lf` on Linux** (the Windows build produces
CRLF only as a side effect of opening the file in text mode, `Valsfile.cpp:72`), `version =
9.0`, `final_newline = true`, and `Num.raw = None` everywhere so `%g` renders canonically.
Windows FxSound reads LF files fine - 13 of the 19 shipped bonus presets are LF.

### 8.4 Write atomically

`valsSave()` truncates in place with `fopen(path, "w")` and has no error recovery; a crash
mid-write destroys the preset. The Rust port should write to `<name>.fac.tmp-<pid>` in the
same directory, `fsync`, then `rename()`.

### 8.5 Recommended test suite

1. `parse()` all 32 shipped files - must succeed, `version == 9`, `num_bands == 10`.
2. `write(parse(x)) == x` for the 27 files listed in §5.2.
3. For the 5 hand-edited files, assert the *only* differences are `%g`-unrepresentable
   CF tokens, and that the `Num.raw` passthrough makes them byte-identical too.
4. Golden values for `Default.fac` - all mains `0`, all boosts `0`, CFs
   `62.5, 121.5, 225, 416.5, 770.5, 1425, 2645, 4895, 9060, 13885`.
5. `midi_to_value(127) == 1.0` exactly; `value_to_midi(midi_to_value(m)) == m` for all
   `m in 0..=127`.
6. Fuzz `parse()` - it must never panic, never allocate unboundedly on a hostile
   `Total number of elements` or `Number of EQ Bands` (clamp to `VALS_MAX_NUM_ELEMENTS`
   and `EQ_MAX_NUM_BANDS` *before* `with_capacity`).

---

## 9. Windows-specific machinery and its Linux replacement

| Windows mechanism | What it achieves | Linux / Wayland / PipeWire replacement |
|---|---|---|
| `%APPDATA%\FxSound\Presets` via `File::getSpecialLocation(userApplicationDataDirectory)` (`FxController.cpp:740, 856`) | per-user writable preset store | `$XDG_DATA_HOME/fxsound/presets` (default `~/.local/share/fxsound/presets`) via the `directories` or `xdg` crate. Create with `0700`. |
| `%APPDATA%\FxSound\AutoSave` (`FxController.cpp:805-806`) | crash-safe scratch copy of a dirty preset | `$XDG_STATE_HOME/fxsound/autosave` (`~/.local/state/fxsound/autosave`) - it is machine-local regenerable state, not user data, so `state` not `data`. |
| `<cwd>\Factsoft` (`FxController.cpp:844-845`) | read-only factory presets next to the .exe | Search order: `$FXSOUND_FACTORY_PRESET_DIR`, then each `$XDG_DATA_DIRS/fxsound/presets/factory`, then a build-time `PREFIX/share/fxsound/presets/factory`. Never `current_dir()`. |
| `Documents\FxSound\Presets\Export\` (`FxController.cpp:1386`) | user-visible export drop | `xdg-user-dir DOCUMENTS` (parse `~/.config/user-dirs.dirs`), else `~/Documents`. Better: a **Wayland-native file dialog** via `ashpd` (`org.freedesktop.portal.FileChooser`), because a sandboxed/Flatpak build has no direct access to `~/Documents`. |
| `SHFileOperation(FO_DELETE, ...)` for delete/rename (`FxController.cpp:1265-1267, 1297-1299, 1363-1365`) | shell delete with Recycle-Bin semantics | `std::fs::remove_file` for a hard delete, or `trash` crate / `org.freedesktop.portal.Trash` for a recoverable one. Prefer the portal in a sandbox. Also: replace the copy-then-delete rename with a real `std::fs::rename` (atomic on the same filesystem). |
| `wchar_t` / UTF-16 preset names everywhere in the DSP API (`DfxDsp.h:30-31`) | Windows-native strings | plain `String` / `&str` (UTF-8). The on-disk name was **already UTF-8** (`Valsfile.cpp:85-87, 320-323`), so this simplifies rather than complicates. |
| `fopen(path, "w")` text-mode CRLF translation (`Valsfile.cpp:72`) | CRLF on disk | write LF explicitly; accept both on read. |
| `MAX_PATH` / `wcscpy_s` into `wchar_t[MAX_PATH]` (`FxController.cpp:1259-1264`) | 260-char path cap, and a latent truncation bug for longer paths | `PathBuf`, no cap. |
| `swprintf(..., L"%s\\%s", dir, file)` (`Valsfile.cpp:69`, `Prelst.cpp:160-162`) | path join | `Path::join`. |
| Registry `HKCU\...\LastUsed\EQ\EQOn` for the EQ on/off that gets *written into* the preset (`dsp/DfxDspEq.cpp:269-282`, read at `:306-313`; used by `createValsFromStateInfo` at `dsp/DfxDspPreset.cpp:290`) | persistent "is EQ enabled" outside the preset | a single TOML/JSON settings file at `$XDG_CONFIG_HOME/fxsound/settings.toml`. Do **not** invent a registry abstraction; the port needs exactly one key here. |
| JUCE `settings_` (`preset`, `max_user_presets`, `power`, `theme_mode`, ...) (`FxController.cpp:194-198, 750, 1082`) | app preferences | same `settings.toml`. Keep `max_user_presets` with its `10..=120` clamp and `120` default for parity (`FxController.cpp:194-198`). |
| `MessageBox(NULL, L"TTEST", ...)` on DSP init failure (`dsp/DfxDspPrivate.cpp:77`) | debug leftover | drop it; log via `tracing`. |
| Per-device preset memory (`DeviceConfig::getDeviceConfig(settings_, getOutputName())`, `FxController.cpp:1303-1310, 1371-1378`) | remember which preset was last used per output device | key the same map on the PipeWire **node name** (`node.name`) or the `device.id`/serial of the sink, not on a human-readable description, which is localised and unstable. |
| `DFXP_GRAPHIC_EQ_NUM_BANDS` mutable global (`GraphicEqSet.cpp:154`) | live band count shared between GUI and DSP | an `AtomicUsize` or, better, a value owned by the DSP graph and mirrored into the UI via a channel - never a mutable global touched from the audio thread. |

### 9.1 Preset application on the PipeWire side

The preset carries no DSP topology, only 5 scalar effect values plus an N-band parametric
EQ table. The natural Linux realisation is a **PipeWire `filter-chain` node** (or a
`pipewire-filter` written with `pipewire-rs`) whose `builtin` `bq_peaking` sections are
reconfigured from the preset's `(center_hz, boost_db)` pairs with the derived `Q` from
§3.6. Important consequences for preset handling:

* **Never rebuild the graph on the audio thread.** Preset loads arrive from the UI thread;
  compute biquad coefficients there and hand them across a lock-free SPSC ring (e.g.
  `rtrb`), then cross-fade or snap on the next process cycle. The C++ does the naive thing
  (`GraphicEqSetBandBoostCut` reaches straight into the SOS coefficient arrays,
  `GraphicEqSet.cpp:304-310`); do not copy that.
* Clamp `center_hz * 2 < sample_rate` per band, matching the C++ bypass rule
  (`GraphicEqSet.cpp:288-295`), and recompute on sample-rate change
  (`GraphicEqReCalcAllBandCoeffs`, `GraphicEqSet.cpp:321-352`) - PipeWire will renegotiate
  rate underneath you.
* The preset's `eq_on` flag maps to bypassing the EQ section, not to unloading the node.

### 9.2 Things that simply do not exist on Wayland

* There is **no** global hotkey API for a Wayland client. FxSound's Windows hotkey path
  (outside this subsystem) must be delegated to the compositor - ship a documented
  `.desktop` action plus a D-Bus method, or register with
  `org.freedesktop.portal.GlobalShortcuts` where the compositor implements it. Preset
  next/previous is the obvious binding.
* `SHFileOperation`'s Recycle Bin has no Wayland analogue in-process; use the Trash portal.
* There is no `HWND` to hang a modal on; the export "overwrite?" confirmation
  (`FxController.cpp:1403`) becomes an `egui::Window` with `.modal(true)` or a portal
  dialog.

---

## 10. Layout of the preset UI (for reference)

The preset row is a combo plus four icon buttons; exact pixel geometry belongs to the
GUI spec, but the *states* the preset subsystem must expose are:

```
+--------------------------------------------------------------------+
|  [ Preset name                                  v ]  [*]           |   * = modified marker
|                                                                    |     (FxModel::isPresetModified)
|   ( Save )  ( Save as )  ( Rename )  ( Delete )                    |
|      ^          ^            ^           ^                         |
|      |          |            |           +-- enabled iff UserPreset |
|      |          |            +-------------- enabled iff UserPreset |
|      |          +--------------------------- disabled at max_user_presets (120)
|      +-------------------------------------- enabled iff modified   |
+--------------------------------------------------------------------+
```

* `modified` is true iff `AutoSave/<name>.fac` exists (`FxController.cpp:869-873`).
* "Save" on a *factory* preset writes a same-named file into the user dir, shadowing it
  (`FxController.cpp:1215-1216`) - decide deliberately whether to keep that behaviour.
* Rename and Delete are hard-gated to `PresetType::UserPreset`
  (`FxController.cpp:1254, 1285`).

---

## Open questions / risks for the Rust port

1. **Byte-identical write-back is achievable but only with a `Num.raw` passthrough.**
   Verified: 27/32 shipped files round-trip byte-for-byte with a pure `%g` writer once
   newline style and final-newline presence are preserved; the remaining 5
   (`Factsoft/2,3,5,8,9.fac`) contain hand-edited `NNN.0` frequency tokens that C's `%g`
   cannot emit. If the port ever normalises those (as Windows FxSound's own *export* path
   already does, `dsp/DfxDspPreset.cpp:117-135`), the files change size but not meaning.
   **Decision needed:** preserve raw tokens forever, or accept one-time normalisation?

2. **C `%g` must be reimplemented exactly for newly saved presets.** Rust's `f32` Display
   is not `%g`. Getting this subtly wrong produces presets that Windows FxSound still reads
   (its `%g` scan is lenient) but that diff noisily against the originals. Either link
   `libc::snprintf` or write and fuzz a `fmt_g`.

3. **`realtype` is `float` - confirmed**, `#define realtype float` at
   `dsp/ptutil/include/codedefs.h:150`. So every `%g` field (version, `Real[i]`, `CF`,
   `Boost/Cut`) is single-precision and the port must use `f32`, not `f64`. Using `f64`
   would change the `%g` rendering of values such as `4.72441` (which is `600/127` rounded
   to `f32`) and break byte-identical round-trips. Risk: any Rust code path that widens to
   `f64` for arithmetic must narrow back to `f32` before serialising.

4. **The stored per-effect on/off flags are dead in practice.** `setEffectValue()` derives
   them from `value != 0` immediately after every load
   (`dsp/DfxDspPrivate.cpp:295-302` driven from `FxController.cpp:1085-1089`), so
   `Integer[0..4]` never changes behaviour. Preserving them on write is cheap and correct;
   *honouring* them would change the sound of `Metal.fac` / `R&B.fac` (ambience flag `0`,
   ambience value `38`). **Recommend: preserve, do not honour** - matching Windows.

5. **`music_mode` is written but ignored** (hard-coded to `2`,
   `dsp/DfxDspPreset.cpp:236-242`). If the Linux port ever revives Music1/Speech modes it
   will need DSP that this source tree does not expose.

6. **Preset name vs filename can disagree**, and `isPresetNameValid()` is a
   case-insensitive uniqueness check across *all* presets, factory included
   (`FxModel.cpp:142-153`). On a case-sensitive Linux filesystem `Jazz.fac` and `jazz.fac`
   can coexist on disk but not in the model - one will silently be skipped on import.
   Decide the collision policy explicitly.

7. **Name characters.** `/` and NUL are legal in a preset name but not in a filename;
   `R&B.fac` shows that shell-significant characters are already in use. Sanitise the
   filename, never the name, and handle the resulting name/filename divergence.

8. **Enumeration order is undefined** (JUCE `findChildFiles` returns directory order,
   `FxController.cpp:846, 858`). Any deterministic order the port picks will differ from
   Windows. Pick one, document it, and expose a user sort.

9. **Non-monotonic and coincident band frequencies exist in shipped data**
   (`540`/`541` Hz, `1000`/`1010` Hz - §7.1). Do not sort, dedupe, or validate them away.
   Two coincident peaking filters at the same frequency and gain are a legitimate
   (if odd) 12 dB shelf; the sound of those presets depends on it.

10. **Band-count remapping is lossy and asymmetric** (linear interpolation upward,
    nearest-pick downward, §3.7). A user who switches 10 -> 31 -> 10 bands does **not** get
    their original curve back. Consider keeping the preset's authored band table
    untouched in the model and only remapping at DSP-apply time.

11. **`FxController::setPreset()` calls the band setters with 0-based indices**
    (`FxController.cpp:1092-1095`) while `GraphicEqSetBandFreq()` rejects `0`
    (`GraphicEqSet.cpp:550-551`). The top band is silently never refreshed on Windows.
    Fixing this in the port is correct but is an **audible behaviour change** on preset
    load - flag it in the changelog.

12. **`LINE_LENGTH 128` truncation is a real compatibility cliff.** A preset name longer
    than 127 UTF-8 bytes, written by a permissive Linux port, will desynchronise
    `valsRead()` on Windows and corrupt the entire parse with no error. Enforce
    `MAX_PRESET_NAME_LEN = 128` bytes (`dsp/DfxDspPrivate.cpp:43`) *in bytes, after UTF-8
    encoding*, on save.

13. **Auto-save semantics leak into enumeration.** A preset is "modified" iff an auto-save
    file with a matching *name* exists. Two presets with the same name in different
    directories (factory + user shadow) share one auto-save slot
    (`FxController.cpp:809-812`). That is a genuine bug on Windows; the port should key
    auto-saves by a stable id (hash of full path) instead.

14. **No integrity checking at all.** No checksum, no length, no version-range guard on the
    EQ band count beyond `GraphicEqNew()`'s `1..=32`. A truncated or hostile `.fac` from an
    import can drive huge allocations. Clamp `total_num_elements` to
    `VALS_MAX_NUM_ELEMENTS = 8` and band count to `32` *before* allocating, and fuzz the
    parser.

15. **The whole `vals` module is shared with a wider "effect file" concept**
    (`valsCfgRead`/`valsCfgWrite`, `valsWarp*` in `dsp/ptutil/VALS/Valscfg.cpp` and
    `Valswarp.cpp`) that DFX presets never touch. Those were not read for this spec and are
    out of scope; if a `.cfg`-style companion file ever turns up in a user's data directory,
    it is not a preset.

16. **`Installer/Resources/Factsoft/Default.fac` is not enumerated by
    `bin/{x64,x86,arm64}/Factsoft/`** - the runtime copies ship only `1.fac`..`12.fac`
    (12 files) while the installer resource dir has 13. Decide whether the Linux package
    ships `Default.fac` (an all-zero, flat-EQ "no processing" preset, useful as a
    guaranteed-safe fallback) or matches the runtime set.
