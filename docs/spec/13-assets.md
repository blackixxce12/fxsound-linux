# 13 — Binary assets & internationalised strings

Reverse-engineering spec for the FxSound Windows app (JUCE 6.1.6, `FxSound.jucer` v1.2.15.0) →
native Linux Rust 1.98.1 / egui-eframe 0.36 / winit-Wayland / PipeWire.

Every number below was read out of a file in the repo and is cited as `path:line`. Where no line
exists (binary blobs), the citation is `path` plus the measurement command that produced the value.

Source-of-truth files for this subsystem:

| File | Role |
|---|---|
| `fxsound/FxSound.jucer` | x86/x64 Projucer project; declares every embedded resource |
| `fxsound/FxSoundARM.jucer` | ARM64 project; same resource list **plus 4 extra image files** |
| `fxsound/JuceLibraryCode/BinaryData.h` / `.cpp` | Projucer-generated blob store, 100 resources |
| `fxsound/Source/GUI/FxTheme.h` / `.cpp` | `FxImage` slot enum + per-theme image tables + font loading |
| `fxsound/Source/GUI/FxLanguage.cpp` | Language picker widget, the canonical shipped-language list |
| `fxsound/Source/GUI/FxController.cpp` | `setLanguage()` / `getLanguageName()` — string-table dispatch |
| `fxsound/Images/` | 69 `.svg` + 6 `.png` |
| `fxsound/Fonts/` | 17 font files compiled or shipped with the GUI |
| `Installer/Resources/Fonts/` | 20 font files installed next to the exe and loaded at runtime |
| `Resources/Strings/` | **empty in this checkout** — the 30 `.txt` string tables exist only inside `BinaryData.cpp` |

---

## 1. Asset inventory at a glance

```
fxsound-app-main/
├── fxsound/
│   ├── Images/            69 svg  +  6 png          969 431 B svg, 97 775 B png on disk
│   ├── Fonts/             17 files                58 328 380 B
│   ├── Project/           5 .ico + 3 .rc
│   ├── ProjectARM/        5 .ico + 3 .rc (same .ico bytes)
│   └── JuceLibraryCode/
│       ├── BinaryData.h   100 resource declarations
│       └── BinaryData.cpp 1 688 208 B generated blob
├── Resources/             EMPTY (Resources/Strings/*.txt referenced by .jucer:236-268 are absent)
├── Installer/Resources/
│   ├── Fonts/             20 files                80 033 360 B  ← runtime-loaded, not embedded
│   ├── Factsoft/          12 × N.fac + Default.fac
│   ├── fxsound.ico, dfx.ico, FxSound.settings
├── bin/
│   ├── x86/ x64/ arm64/   fxdiag.exe + FxSound.pdb + Factsoft/1..12.fac
│   ├── x86/Win7/          FxSound.pdb
│   └── BonusPresets/      20 × *.fac + BonusPresets.zip + MeaningfulPresets (a text blurb)
└── release/               fxsound_setup.exe, changelog.txt, updates.txt (+ arm64/)
```

Bytes actually linked into the executable via `BinaryData` (sum of the `…Size` constants in
`fxsound/JuceLibraryCode/BinaryData.h`):

| Class | Count | Bytes |
|---|---:|---:|
| SVG | 65 | 95 575 |
| TTF (Gilroy only) | 3 | 251 704 |
| PNG | 2 | 3 722 |
| Localisation `.txt` | 30 | 389 339 |
| **Total** | **100 resources** | **740 340** |

The 4 on-disk SVGs that are **not** embedded are `button-normal.svg`, `button-hover.svg`,
`button-depressed.svg` (not in either `.jucer`) and `equalizer.svg` (`FxSound.jucer:38`,
`resource="0"`).

> `BinaryData.h:312` — `const int namedResourceListSize = 100;`

Everything else (`Installer/Resources/Fonts/*`, `bin/**/Factsoft/*.fac`, `*.ico`) ships as loose
files beside the executable and is opened at runtime.

### 1.1 JUCE name-mangling rules (needed to read `BinaryData.h`)

Projucer derives the C identifier from the filename: `.` → dropped, `-` → dropped, space → `_`,
extension `.` → `_`.

| Original filename | `BinaryData` symbol | Citation |
|---|---|---|
| `FxSound Black Bars.svg` | `FxSound_Black_Bars_svg` | `BinaryData.h:113` |
| `logo-red.svg` | `logored_svg` | `BinaryData.h:71` |
| `logo-white.svg` | `logowhite_svg` | `BinaryData.h:74` |
| `Slider_Thumb_bw.svg` | `Slider_Thumb_bw_svg` | `BinaryData.h:218` |
| `FxSound.zh-CN.txt` | `FxSound_zhCN_txt` | `BinaryData.h:305` |
| `FxSound.pt-br.txt` | `FxSound_ptbr_txt` | `BinaryData.h:272` |
| `fxsound.hu.txt` (lower-case f!) | `fxsound_hu_txt` | `BinaryData.h:248` |

### 1.2 CRLF caveat

Declared sizes in `BinaryData.h` are 6–15 bytes larger than the on-disk files, e.g.
`remove.svg` is 263 B on disk but `remove_svgSize = 269` (`BinaryData.h:21`). The blob was generated
from CRLF-terminated originals; this checkout has LF. **A Rust port that re-embeds the on-disk files
will therefore not byte-match the shipped blob — this is expected and harmless.** JUCE also appends
two `\0` bytes past the declared length for byte-array resources.

---

## 2. The theme/image indirection (`FxTheme`)

All chrome images go through a 2-D table indexed by `[theme_mode][FxImage]`.

`fxsound/Source/GUI/FxTheme.h:28` declares the mode enum:

```cpp
enum FxThemeMode : int {Dark=0, Light, NumModes};
```

`FxTheme.h:32-37` declares the 32-slot image enum (order is load-bearing — the tables at
`FxTheme.cpp:31-59` are positional):

```cpp
enum FxImage : int { DefaultLogo, HighlightedLogo, IconLogo,
   PowerOnButton, PowerOffButton, DonateButton, DonateButtonHover, MenuButton, MenuButtonHover,
   MinimizeButton, MinimizeButtonHover, MaximizeButton, MaximizeButtonHover,
   MinimizeWindowButton, MinimizeWindowButtonHover,
   FlipButton, FlipButtonHover, RestoreDefaultsButton, RestoreDefaultsButtonHover, RemoveButton,
   ArrowNext, ArrowNextBW, ArrowPrev, ArrowPrevBW,
   ArrowUpSelected, ArrowUp, ArrowDownSelected, ArrowDown, DropDownArrow, DropDownArrowHover,
   SliderThumb, SliderThumbBW, NumImages };
```

Accessors (`FxTheme.cpp:505-513`, macros at `FxTheme.h:118-120`):

```cpp
#define FXIMAGE(image)     (FxTheme::getImage(FxImage::image))       // const char* SVG text
#define FXIMAGESIZE(image) (FxTheme::getImageSize(FxImage::image))
```

Default mode is Dark: `FxThemeMode FxTheme::theme_mode_ = FxThemeMode::Dark;` (`FxTheme.cpp:61`).

### 2.1 Complete slot → file map (the single most important table in this document)

Dark column from `FxTheme.cpp:32-37`; Light column from `FxTheme.cpp:39-44`.

| # | `FxImage` slot | Dark (mode 0) file | Light (mode 1) file | Dark ink | Light ink |
|---:|---|---|---|---|---|
| 0 | `DefaultLogo` | `logo-white.svg` | `logo-black.svg` | `#fff` | `#000` |
| 1 | `HighlightedLogo` | `logo-red.svg` | `logo-blue.svg` | `#e63462` | `#23B6EB` |
| 2 | `IconLogo` | `FxSound White Bars.svg` | `FxSound Black Bars.svg` | `#fff` | `#000` |
| 3 | `PowerOnButton` | `power_on.svg` | `power_on_blue.svg` | `#E63462` | `#23b6eb` |
| 4 | `PowerOffButton` | `power_off.svg` | `power_off_black.svg` | `#FFFFFF` | `#000000` |
| 5 | `DonateButton` | `donate.svg` | `donate_blue.svg` | `#E63462` | `#23B6EB` |
| 6 | `DonateButtonHover` | `donate_hover.svg` | `donate_hover_blue.svg` | `#E63462` | `#23B6EB` |
| 7 | `MenuButton` | `menu.svg` | `menu_black.svg` | `#FFFFFF` | `#000000` |
| 8 | `MenuButtonHover` | `menu_hover.svg` | `menu_hover_blue.svg` | `#E63462` | `#23B6EB` |
| 9 | `MinimizeButton` | `minimize.svg` | `minimize_black.svg` | `#FFFFFF` | `#000000` |
| 10 | `MinimizeButtonHover` | `minimize_hover.svg` | `minimize_hover_blue.svg` | `#E63462` | `#23b6eb` |
| 11 | `MaximizeButton` | `maximize.svg` | `maximize_black.svg` | `#FFFFFF` | `#000000` |
| 12 | `MaximizeButtonHover` | `maximize_hover.svg` | `maximize_hover_blue.svg` | `#E63462` | `#23B6EB` |
| 13 | `MinimizeWindowButton` | `min_window.svg` | `min_window_black.svg` | `#FFFFFF` | `#000000` |
| 14 | `MinimizeWindowButtonHover` | `min_window_hover.svg` | `min_window_hover_blue.svg` | `#E63462` | `#23B6EB` |
| 15 | `FlipButton` | `flip_white.svg` | `flip_black.svg` | `#FFFFFF` | `#000000` |
| 16 | `FlipButtonHover` | `flip.svg` | `flip_blue.svg` | `#E63462` | `#23B6EB` |
| 17 | `RestoreDefaultsButton` | `restore_defaults_white.svg` | `restore_defaults_black.svg` | `#FFFFFF` | `#000000` |
| 18 | `RestoreDefaultsButtonHover` | `restore_defaults.svg` | `restore_defaults_blue.svg` | `#E63462` | `#23B6EB` |
| 19 | `RemoveButton` | `remove.svg` | `remove.svg` *(shared)* | `#D51535` | `#D51535` |
| 20 | `ArrowNext` | `arrow_next.svg` | `arrow_next_blue.svg` | `#E63462` | `#23B6EB` |
| 21 | `ArrowNextBW` | `arrow_next_bw.svg` | `arrow_next_bw.svg` *(shared)* | `#B0B0B0` | `#B0B0B0` |
| 22 | `ArrowPrev` | `arrow_prev.svg` | `arrow_prev_blue.svg` | `#E63462` | `#23B6EB` |
| 23 | `ArrowPrevBW` | `arrow_prev_bw.svg` | `arrow_prev_bw.svg` *(shared)* | `#B0B0B0` | `#B0B0B0` |
| 24 | `ArrowUpSelected` | `arrow_up.svg` | `arrow_up_blue.svg` | `#E63462` | `#23B6EB` |
| 25 | `ArrowUp` | `arrow_up_white.svg` | `arrow_up_black.svg` | `#B1B1B1` | `#4E4E4E` |
| 26 | `ArrowDownSelected` | `arrow_down.svg` | `arrow_down_blue.svg` | `#E63462` | `#23B6EB` |
| 27 | `ArrowDown` | `arrow_down_white.svg` | `arrow_down_black.svg` | `#B1B1B1` | `#4E4E4E` |
| 28 | `DropDownArrow` | `dropdown_arrow_bw.svg` | `dropdown_arrow_bw.svg` *(shared)* | `#B0B0B0` | `#B0B0B0` |
| 29 | `DropDownArrowHover` | `dropdown_arrow_hover.svg` | `dropdown_arrow_hover_blue.svg` | `#E63462` | `#23B6EB` |
| 30 | `SliderThumb` | `Slider_Thumb.svg` | `Slider_Thumb_blue.svg` | gradient (see §3.4) | gradient |
| 31 | `SliderThumbBW` | `Slider_Thumb_bw.svg` | `Slider_Thumb_bw.svg` *(shared)* | grey gradient | grey gradient |

Note the **counter-intuitive naming at slots 24–27**: `ArrowUpSelected` is the *accent* (red/blue)
variant, `ArrowUp` is the *grey* variant. In `FxOutputPreference.cpp:34` the accent asset is passed
as the JUCE "over" (hover) image:

```cpp
up_button_.setImages(up_image_.get(), up_selected_image_.get(), up_image_.get());
```

### 2.2 Theme-variant naming convention

Distilled from the 69 filenames and §2.1:

| Suffix | Meaning | Where it appears in the table |
|---|---|---|
| *(none)* | Dark-theme **rest** state for _white-inked_ icons (`menu`, `minimize`, `maximize`, `min_window`, `power_off`), **or** Dark-theme **accent** for red-inked icons (`donate`, `arrow_next`, `arrow_prev`, `arrow_up`, `arrow_down`, `power_on`, `flip`, `restore_defaults`) | inconsistent — must be read per-slot |
| `_white` | white `#FFFFFF` fill — Dark rest for `flip`, `restore_defaults`; light-grey `#B1B1B1` for `arrow_*` | slots 15, 17, 25, 27 |
| `_black` | black `#000000` fill — Light rest; `#4E4E4E` for `arrow_*` | slots 4, 7, 9, 11, 13, 15, 17, 25, 27 |
| `_blue` | `#23B6EB` accent — **Light-theme** accent/hover | slots 1, 3, 5, 16, 18, 20, 22, 24, 26 |
| `_hover` | Dark hover (`#E63462`) | slots 6, 8, 10, 12, 14, 29 |
| `_hover_blue` | Light hover (`#23B6EB`) | slots 6, 8, 10, 12, 14, 29 |
| `_bw` | theme-independent disabled/greyed `#B0B0B0` | slots 21, 23, 28, 31 |
| *(no suffix, shared)* | `remove.svg` `#D51535` in both themes | slot 19 |

> **The suffix set is `{"", _black, _white, _blue, _hover, _hover_blue, _bw}` — there is no
> `_hover_white` and no `_blue_hover`.** Two families invert the convention (`flip` /
> `restore_defaults`: unsuffixed = accent, `_white` = Dark rest), so a Rust port must transcribe
> §2.1 literally rather than deriving filenames from a rule.

The accent colours line up exactly with the theme colour table at `FxTheme.cpp:22-29`:
`ImageButton` = `0xe63462` (Dark, `FxTheme.cpp:24`) and `0x23b6eb` (Light, `FxTheme.cpp:29`);
`DefaultText` = `0xb1b1b1` / `0x4e4e4e` (`FxTheme.cpp:23` / `:27`).

---

## 3. Per-asset geometry and consumers

### 3.1 SVG intrinsic sizes

All 69 SVGs, grouped by intrinsic `viewBox`:

| viewBox | width×height attr | Files |
|---|---|---|
| `0 0 6 5` | 6×5 | `arrow_down.svg`, `arrow_down_black.svg`, `arrow_down_blue.svg`, `arrow_down_white.svg`, `arrow_up.svg`, `arrow_up_black.svg`, `arrow_up_blue.svg`, `arrow_up_white.svg` |
| `0 0 7 11` | 7×11 | `arrow_next*.svg` (3), `arrow_prev*.svg` (3) |
| `0 0 11 7` | 11px×7px | `dropdown_arrow_bw.svg`, `dropdown_arrow_hover.svg`, `dropdown_arrow_hover_blue.svg` |
| `0 0 14 10` | 14px×10px | `menu*.svg` (4), `min_window*.svg` (4) |
| `0 0 16 16` | 16px×16px | `flip*.svg` (4), `maximize*.svg` (4), `remove.svg`, `restore_defaults*.svg` (4), `Slider_Thumb_bw.svg` |
| `0 0 18 18` | 18px×18px | `minimize*.svg` (4) |
| `0 0 24 24` | 24×24 | `equalizer.svg`, `question.svg`, `settings.svg`, `speaker.svg` |
| `0 0 30 31` | 30px×31px | `donate*.svg` (4), `power_off*.svg` (2), `power_on*.svg` (2) |
| `0 0 64 64` | 64px×64px | `Slider_Thumb.svg`, `Slider_Thumb_blue.svg` |
| `0 0 100 100` | 100px | `Button_OFF.svg` *(unused)* |
| `0 0 102 102` | 102px | `Button_ON.svg` *(unused)* |
| `0 0 299.83 219.26` | — (no w/h) | `FxSound White Bars.svg`, `FxSound Black Bars.svg` |
| `0 0 526.19 75.15` | — (no w/h) | `logo-white.svg`, `logo-black.svg`, `logo-red.svg`, `logo-blue.svg`, `FxSound Logo.svg` |
| `334.28… 333.79… 69 69` | 65×65 | `button-depressed.svg` *(unused, 291 238 B)* |
| `250.28… 371.79… 69 69` | 65×65 | `button-hover.svg` *(unused, 291 250 B)* |
| `251.28… 292.79… 69 69` | 65×65 | `button-normal.svg` *(unused, 291 249 B)* |

`FxSound Logo.svg` is **byte-identical** to `logo-white.svg` (both md5 `f167566c0bddd79554de6f6c632fd4a3`)
and is embedded twice (`BinaryData.h:173-174` `FxSound_Logo_svgSize = 7254` vs `:74-75` `logowhite_svgSize = 7254`).

### 3.2 Where each asset is drawn, and at what pixel size

| Asset / slot | Consumer | Component box | Actual drawn size | Citation |
|---|---|---|---|---|
| `DefaultLogo` (logo-white/black, viewBox 526.19×75.15) | main-window title bar, no window name | `ICON_WIDTH=106`, `ICON_HEIGHT=15` | **106×15 px**, x-left / y-mid inside a 57 px title bar | `FxWindow.h:71-72`, `FxWindow.cpp:364-366` |
| `HighlightedLogo` (logo-red/blue) | title-bar logo cross-fade while audio is processing | same 106×15 | 106×15; 600 ms `fadeIn`/`fadeOut` | `FxWindow.cpp:368-370`, `:197-207` |
| `IconLogo` (FxSound White/Black Bars, 299.83×219.26) | title bar of **named** windows (Settings, dialogs) | height `ICON_HEIGHT-1 = 14`, width = `14 * w/h` ⇒ **≈19×14 px** | aspect-preserved | `FxWindow.cpp:374-377`, `:280-281` |
| `DefaultLogo` | toast notification icon | `ICON_WIDTH=79`, `ICON_HEIGHT=12` at offset (15,10) | **79×12 px** | `FxNotification.h:42-43`, `FxNotification.cpp:48-50` |
| `PowerOnButton` / `PowerOffButton` (30×31) | `FxPowerButton` in title bar | `BUTTON_WIDTH=24` | **24×24 px**, `stretchToFit|centred`, alpha 0.5 when disabled | `FxMainWindow.h:54`, `:192-193`, `FxPowerButton.cpp:32-48` |
| `MenuButton` / `MenuButtonHover` (14×10) | hamburger, left-aligned toolbar | 24×24 `ImageFitted` | fitted into **24×24** → drawn ≈14×10 centred | `FxMainWindow.cpp:199`, `:329-331` |
| `DonateButton` / `…Hover` (30×31) | title bar, right-aligned | `BUTTON_WIDTH+2 × BUTTON_WIDTH+6` = **26×30** | `ImageFitted` | `FxMainWindow.cpp:210`, `:333-335` |
| `MinimizeWindowButton` / `…Hover` (14×10) | "minimise to tray" | **26×30** (`BUTTON_WIDTH+2 × +6`) | `ImageFitted` | `FxMainWindow.cpp:220`, `:337-339` |
| `MinimizeButton` / `…Hover` (18×18) | resize button while in **Pro** view (collapses to Lite) | **26×26** (`BUTTON_WIDTH+2`) | `ImageFitted` | `FxMainWindow.cpp:353-355` |
| `MaximizeButton` / `…Hover` (16×16) | resize button while in **Lite** view (expands to Pro) | **24×24** | `ImageFitted` | `FxMainWindow.cpp:359-361` |
| `FlipButton` / `…Hover` (16×16) | flip between effects page and EQ page | `BUTTON_WIDTH=18`, `BUTTON_HEIGHT=18` at `(W-18-5, 5)` | **18×18** | `FxAudioControls.h:149-150`, `.cpp:83`, `:70-73` |
| `RestoreDefaultsButton` / `…Hover` (16×16) | EQ panel "restore defaults" | 18×18 | **18×18** | `FxAudioControls.h:103-104`, `.cpp:459`, `:424-427` |
| `RemoveButton` (`remove.svg`, 16×16) | output-preference row delete | `BUTTON_WIDTH=18` | **18×18** | `FxOutputPreference.h:35`, `.cpp:49`, `:129` |
| `ArrowUp` / `ArrowUpSelected` / `ArrowDown` / `ArrowDownSelected` (6×5) | output-preference row reorder | 18×18 `ImageFitted` | fitted into **18×18** → drawn 6×5 centred | `FxOutputPreference.cpp:26-29`, `:33`, `:41` |
| `ArrowNext` / `ArrowNextBW` / `ArrowPrev` / `ArrowPrevBW` (7×11) | language ◀ ▶ stepper in Settings→General | `BUTTON_WIDTH=14`, `BUTTON_HEIGHT=22` | fitted into **14×22** | `FxLanguage.h:36-37`, `.cpp:30-36`, `:42-43` |
| `DropDownArrow` / `DropDownArrowHover` (11×7) | every `ComboBox` | `Rectangle<float>(width - margin, 0, 12, height)` where `margin = 32`, or `24` when `width <= 150` | drawn inside a **12 px-wide** column, `RectanglePlacement::centred` | `FxTheme.cpp:155-163` |
| `SliderThumb` / `SliderThumbBW` (64×64 / 16×16) | linear + rotary sliders, balance slider | `SLIDER_THUMB_RADIUS = 8` ⇒ **16×16**; `ROTARY_SLIDER_THUMB_RADIUS = 5` ⇒ **10×10** | `drawWithin(..., radius*2, radius*2)` | `FxTheme.h:44-45`, `.cpp:206`, `:240`, `:315`, `:320-330`, `FxBalanceSlider.cpp:27-28`, `:154-155` |
| `speaker.svg` (24×24) | Settings dialog left rail, "Audio" tab | button `150×40`, icon rect = `40×40` reduced by `(10,10)` ⇒ **20×20** | `FxSettingsDialog.cpp:94`, `:58-61`, `.h:203-204` |
| `settings.svg` (24×24) | Settings dialog left rail, "General" tab | **20×20** | `FxSettingsDialog.cpp:100` |
| `question.svg` (24×24) | Settings dialog left rail, "Help" tab | **20×20** | `FxSettingsDialog.cpp:105` |

The three tab icons are loaded **directly from `BinaryData`, not through `FXIMAGE`**, so they stay
`#7E7E7E` in both themes:

```cpp
audio_button_->setImage(Drawable::createFromImageData(BinaryData::speaker_svg,   BinaryData::speaker_svgSize).get());   // FxSettingsDialog.cpp:94
general_button_->setImage(Drawable::createFromImageData(BinaryData::settings_svg, BinaryData::settings_svgSize).get()); // :100
help_button_->setImage(Drawable::createFromImageData(BinaryData::question_svg,    BinaryData::question_svgSize).get()); // :105
```

### 3.3 Title-bar layout geometry (for reproducing icon positions)

```
FxTheme::TITLE_BAR_HEIGHT = 57          (FxTheme.h:43)
FxTheme::WINDOW_CORNER_RADIUS = 21      (FxTheme.h:42)
FxWindow::SHADOW_WIDTH = 5              (FxWindow.h:44)      (main window sets shadow 0 — FxMainWindow.cpp:183)
FxWindow::CLOSE_BUTTON_WIDTH = 15       (FxWindow.h:45)
title_bar height  = TITLE_BAR_HEIGHT - 1 = 56                (FxWindow.cpp:29)
title_bar bounds  = (21 + shadow, shadow, W - 42 - 2*shadow, 56)   (FxWindow.cpp:147)

  ┌───────────────────────────── title bar, 56 px tall ─────────────────────────────┐
  │ [logo 106×15]   … [menu 24×24]      …    [donate 26×30][power 24][resize][min] [×15]│
  │ x_left = icon_w + 15, then +w+20 each     x_right = 15 + 20, then +w+20 each       │
  └─────────────────────────────────────────────────────────────────────────────────┘
       left-aligned, y-mid                       right-aligned, y-mid
```

Right-aligned order of insertion (`FxMainWindow.cpp:230-234`) — first inserted sits closest to the
close button: `minimize_button_`, `resize_button_`, `power_button_`, `donate_button_`.
`menu_button_` is the only left-aligned one (`addToolbarButton(&menu_button_, false)`).
Stride between toolbar buttons is `button_width + 20` (`FxWindow.cpp:300`, `:306`).

### 3.4 `Slider_Thumb.svg` is not a simple shape

`fxsound/Images/Slider_Thumb.svg` is a 64×64 Sketch export whose payload is a 16 px circle
surrounded by SVG filter effects:

* `linearGradient-1` `#D9304F → #DC3253` (0%→100%, diagonal) — `Slider_Thumb.svg:5-8`
* `linearGradient-2` `#D52F4E → #A41A28` — `Slider_Thumb.svg:9-12`
* outer drop shadow: `feOffset dy=4` + `feGaussianBlur stdDeviation=12` + `feColorMatrix` alpha `0.479048295`, RGB `(0.807843137, 0.168627451, 0.278431373)` — `Slider_Thumb.svg:14-19`
* inner shadow: `feGaussianBlur stdDeviation=2.5`, `feOffset dx=2 dy=2`, alpha `0.6`, RGB `(0.502377717, 0.102867818, 0)` — `Slider_Thumb.svg:20-25`
* inner 3 px `circle cx=8 cy=8 r=3` with a `stdDeviation=0.5`, `dy=1`, alpha `1.0` inner shadow, RGB `(0.0613170393, 0.0719285724, 0.0804177989)` — `Slider_Thumb.svg:26-33`
* base plate colour `#0F0F0F` (matches `ControlBackground` Dark `0x0f0f0f`, `FxTheme.cpp:24`)

`Slider_Thumb_blue.svg` is the same construction with `#063545 / #0A4D66 / #0D5F7E` and a `#f0f0f0`
highlight; `Slider_Thumb_bw.svg` is 16×16 with `#0F0F0F / #7B7B7B / #818181 / #9D9D9D / #9F9F9F`.

**Porting note:** JUCE's own SVG parser renders these filters only approximately. `resvg` implements
`feGaussianBlur`/`feOffset`/`feComposite`/`feColorMatrix` correctly, so the Rust port will actually
look *better* than the original here. Render the thumbs once at 2× or 3× the target (16 px → 32/48 px)
and cache, because filter evaluation is expensive.

### 3.5 Hover variants are mostly stroke-weight changes

`donate.svg` vs `donate_hover.svg` differ only in `stroke-width="1"` → `"2"` (line 3 of each file);
same for `donate_blue.svg` vs `donate_hover_blue.svg`. `menu.svg` uses a 1 px stroked path
(`stroke="#FFFFFF"`, `<path d="M0,1 L14,1 M0,5 L14,5 M0,9 L14,9">`) while `menu_hover.svg` uses a
filled path (`fill="#E63462"`, three 1 px-tall rectangles). Don't assume hover = recolour.

### 3.6 Raster assets

| File | Pixels | Bit depth / colour type | Bytes | Embedded? | Used? |
|---|---|---|---:|---|---|
| `fxsound/Images/fxsound.png` | 32×32 | 8 / RGBA | 388 | yes (`BinaryData.h:179-180`) | **no code reference** |
| `fxsound/Images/fxsound_large.png` | 256×256 | 8 / RGBA | 3 334 | yes (`BinaryData.h:182-183`) | **no code reference** |
| `fxsound/Images/logo-red.png` | 266×40 | 8 / RGBA | 43 192 | no | ARM `.jucer` only |
| `fxsound/Images/logo-white.png` | 266×40 | 8 / RGBA | 43 192 | no | ARM `.jucer` only (**different bytes** from `logo-red.png`: md5 `e77eb235…` vs `e80d5ec1…`) |
| `fxsound/Images/FxSound Logo White.png` | 701×101 | 8 / RGBA | 6 446 | no | ARM `.jucer` only |
| `fxsound/Images/FxSound White Bars.png` | 300×220 | 8 / RGBA | 1 223 | no | ARM `.jucer` only |

Windows icon resources (`fxsound/Project/resources.rc:35-40`, identical bytes under `ProjectARM/`):

| `.ico` | Frames | Resource id | Used for |
|---|---|---|---|
| `icon.ico` | 16, 32, 48, 256 (256 is PNG-compressed) | `IDI_ICON1`, `IDI_ICON2` | exe / window class icon |
| `white_logo.ico` | 16, 24, 32, 48, 256 | `IDI_LOGO_WHITE` | tray + window icon: power on, **not** processing (`FxSystemTrayView.cpp:105`, `FxMainWindow.cpp:386`) |
| `red_logo.ico` | 16, 24, 32, 48, 256 | `IDI_LOGO_RED` | power on + processing + **Dark** theme (`FxSystemTrayView.cpp:96`, `FxMainWindow.cpp:380`) |
| `blue_logo.ico` | **256 only** | `IDI_LOGO_BLUE` | power on + processing + **Light** theme (`FxSystemTrayView.cpp:100`, `FxMainWindow.cpp:382`) |
| `gray_logo.ico` | 16, 24, 32, 48, 256 | `IDI_LOGO_GRAY` | power off (`FxSystemTrayView.cpp:110`, `FxMainWindow.cpp:391`) |

`blue_logo.ico` shipping a single 256×256 frame is a latent Windows bug (a 16×16 tray slot has to
downscale 256→16). **On Linux, generate all four state icons at 16/22/24/32/48/64/128/256 as PNG.**

`Installer/Resources/dfx.ico` (32 988 B) and `Installer/Resources/fxsound.ico` (20 148 B, same size as
`gray_logo.ico`) are installer-only artwork.

### 3.7 Dead / unreferenced assets (do **not** port)

| Asset | Status |
|---|---|
| `button-normal.svg` (291 249 B), `button-hover.svg` (291 250 B), `button-depressed.svg` (291 238 B) | not in either `.jucer`, not in `BinaryData`, not grepped anywhere. 873 737 B of dead weight — **90 % of `Images/` by size**. Their `viewBox` origins are non-zero (`334.28758169934633 333.7908496732027 …`) — clearly a cropped export from a larger canvas. |
| `equalizer.svg` | listed in `FxSound.jucer:38` with `resource="0"` — deliberately *not* compiled into `BinaryData`; no code reference |
| `Button_ON.svg`, `Button_OFF.svg` | embedded (`BinaryData.h:161-166`) but never referenced from any `.cpp` |
| `FxSound Logo.svg` | embedded (`BinaryData.h:173-174`) but never referenced; duplicate of `logo-white.svg` |
| `fxsound.png`, `fxsound_large.png` | embedded but never referenced |
| `logo-red.png`, `logo-white.png`, `FxSound Logo White.png`, `FxSound White Bars.png` | referenced only from `FxSoundARM.jucer` (the ARM project's extra file group); no code reference |

**Arithmetic, verified by grepping `BinaryData::\w+_svg` across `fxsound/Source/`:**

```
 69  .svg files on disk in fxsound/Images/
-  4  never embedded (button-normal, button-hover, button-depressed, equalizer)
────
 65  embedded in BinaryData (65 × …_svgSize constants in BinaryData.h)
-  3  embedded but never referenced (Button_ON, Button_OFF, FxSound Logo)
────
 62  LIVE SVGs
      59  reachable through FxTheme::theme_images_  (32 Dark slots + 32 Light slots,
          of which 5 files are shared between the two themes: remove, arrow_next_bw,
          arrow_prev_bw, dropdown_arrow_bw, Slider_Thumb_bw)
    +  3  loaded directly from BinaryData: speaker.svg, settings.svg, question.svg
```

**Port exactly these 62 files.**

---

## 4. Fonts

### 4.1 `fxsound/Fonts/` — 17 files

| File | Bytes | `name` ID 1 (family) | ID 2 | ID 16 (typographic family) | ID 17 | `usWeightClass` | `fsType` | Embedded in `BinaryData`? |
|---|---:|---|---|---|---|---:|---|---|
| `Gilroy-Regular.ttf` | 84 300 | `Gilroy` | Regular | — | — | 400 | **0x0004** | **yes** (`BinaryData.h:14-15`, size 84 300) |
| `Gilroy-Semibold.ttf` | 83 948 | `Gilroy` | Semibold | — | — | 600 | **0x0004** | **yes** (`BinaryData.h:17-18`, size 83 948) |
| `Gilroy-Bold.ttf` | 83 456 | `Gilroy` | Bold | — | — | 700 | **0x0004** | **yes** (`BinaryData.h:11-12`, size 83 456) |
| `NotoSansArabic-Regular.ttf` | 177 004 | `Noto Sans Arabic` | Regular | — | — | 400 | 0 | no — **and never loaded** (see §4.4) |
| `NotoSansArabic-Medium.ttf` | 177 576 | `Noto Sans Arabic Medium` | Regular | `Noto Sans Arabic` | Medium | — | 0 | no — never loaded |
| `NotoSansKR-Regular.otf` | 4 744 692 | `Noto Sans KR` | Regular | — | — | 400 | 0 | no (runtime file) |
| `NotoSansKR-Medium.otf` | 4 768 768 | `Noto Sans KR Medium` | Regular | `Noto Sans KR` | Medium | — | 0 | no |
| `NotoSansKR-Bold.otf` | 4 909 668 | `Noto Sans KR` | Bold | — | — | — | 0 | no — **never loaded** (KR maps 700→Medium) |
| `NotoSansSC-Regular.otf` | 8 482 020 | `Noto Sans SC` | Regular | — | — | 400 | 0 | no |
| `NotoSansSC-Medium.otf` | 8 508 580 | `Noto Sans SC Medium` | Regular | `Noto Sans SC` | Medium | — | 0 | no |
| `NotoSansSC-Bold.otf` | 8 716 392 | `Noto Sans SC` | Bold | — | — | — | 0 | no — never loaded |
| `NotoSansTC-Regular.otf` | 5 766 468 | `Noto Sans TC` | Regular | — | — | 400 | 0 | no — **wrong extension** (§4.4) |
| `NotoSansTC-Medium.otf` | 5 788 004 | `Noto Sans TC Medium` | Regular | `Noto Sans TC` | Medium | — | 0 | no — wrong extension |
| `NotoSansTC-Bold.otf` | 5 942 628 | `Noto Sans TC` | Bold | — | — | — | 0 | no — never loaded |
| `NotoSansThai-Regular.ttf` | 47 404 | `Noto Sans Thai` | Regular | — | — | 400 | 0 | no |
| `NotoSansThai-Medium.ttf` | 47 472 | `Noto Sans Thai Medium` | Regular | `Noto Sans Thai` | Medium | — | 0 | no |

Only the three Gilroy faces are compiled into the binary. Everything else is read from the process's
**current working directory** at runtime.

### 4.2 `Installer/Resources/Fonts/` — 20 files, the real runtime set

These are the files the MSI drops next to `FxSound.exe` (enumerated in `Installer/fxsound.aip`):

`IBMPlexSansArabic-{Regular,Medium,Bold}.ttf` (226 004 / 231 888 / 236 272 B),
`MontserratAlternates-{Regular,Medium,Bold}.ttf` (201 976 / 199 912 / 201 148 B),
`NotoSansJP-{Regular,Medium,Bold}.ttf` (5 732 824 / 5 729 332 / 5 727 828 B),
`NotoSansKR-{Regular,Medium,Bold}.otf`, `NotoSansSC-{Regular,Medium,Bold}.otf`,
`NotoSansTC-{Regular,Medium,Bold}.ttf` (7 110 796 / 7 106 884 / 7 105 548 B — **`.ttf`, not `.otf`**),
`NotoSansThai-{Regular,Medium}.ttf` (46 380 / 46 448 B — different bytes from `fxsound/Fonts/`).

Family names: `IBM Plex Sans Arabic`, `Montserrat Alternates`, `Noto Sans JP`, `Noto Sans KR`,
`Noto Sans SC`, `Noto Sans TC`, `Noto Sans Thai` (Medium faces expose ID 1 = `<Family> Medium`,
ID 16 = `<Family>`, ID 17 = `Medium`).

### 4.3 Font registration and the three weight slots

`FxTheme` keeps exactly three typefaces (`FxTheme.h:106-108`):

```cpp
Typeface::Ptr font_400_;   // Regular
Typeface::Ptr font_600_;   // Semibold / Medium
Typeface::Ptr font_700_;   // Bold
```

Initialised from `BinaryData` in `init()` (`FxTheme.cpp:95-97`) and re-selected per language in
`loadFont(String language)` (`FxTheme.cpp:382-459`). The last statement of `loadFont` is
`setDefaultSansSerifTypeface(font_600_);` (`FxTheme.cpp:458`) — **Semibold is the app's default
face, not Regular.**

Text sizes actually used:

| API | Face | Height (px) | Citation |
|---|---|---:|---|
| `getNormalFont()` | `font_600_` | **17.0** | `FxTheme.cpp:466-469` |
| `getSmallFont()` | `font_400_` | **14.0** | `FxTheme.cpp:471-474` |
| `getTitleFont()` | `font_700_` | **17.0** | `FxTheme.cpp:476-479` |
| `getPopupMenuFont()` | `font_600_` | **17.0** | `FxTheme.cpp:367-370` |
| `getComboBoxFont()` | `font_600_` | **14.0** if `box.getHeight() <= 30`, else **17.0** | `FxTheme.cpp:120-126` |
| `getTextButtonFont()` | `font_600_` | `min(17.0, button_height)` | `FxTheme.cpp:461-464` |
| tooltip text | `getNormalFont()` @ **14.0**, wrapped at **400 px** max width | `FxTheme.cpp:678-679` |
| document-window title bar | plain `Font(12.0f, Font::plain)` — system face, not Gilroy | `FxTheme.cpp:555` |
| effect labels (Clarity/Ambience/…) | `getNormalFont().withHeight(14)` | `FxAudioControls.cpp:104` |
| notification body | `getSmallFont().withHeight(17.0f)` | `FxNotification.cpp:80` |

### 4.4 Per-language font mapping (`FxTheme::loadFont`)

| Language prefix (case-insensitive) | 400 | 600 | 700 | Citation | Ships? |
|---|---|---|---|---|---|
| `en` | `Gilroy-Regular.ttf` (BinaryData) | `Gilroy-Semibold.ttf` | `Gilroy-Bold.ttf` | `FxTheme.cpp:384-389` | embedded |
| `ko` | `NotoSansKR-Regular.otf` | `NotoSansKR-Medium.otf` | `NotoSansKR-Medium.otf` | `:390-395` | yes |
| `zh-CN` | `NotoSansSC-Regular.otf` | `NotoSansSC-Medium.otf` | `NotoSansSC-Medium.otf` | `:396-401` | yes |
| `zh-TW` | `NotoSansTC-Regular.ttf` | `NotoSansTC-Medium.ttf` | `NotoSansTC-Medium.ttf` | `:402-407` | yes (installer ships `.ttf`) |
| `th` | `NotoSansThai-Regular.ttf` | `NotoSansThai-Medium.ttf` | `NotoSansThai-Medium.ttf` | `:408-413` | yes |
| `vi` | `MontserratAlternates-Regular.ttf` | `MontserratAlternates-Medium.ttf` | `MontserratAlternates-Bold.ttf` | `:414-419` | yes |
| `ja` | `NotoSansJP-Regular.ttf` | `NotoSansJP-Medium.ttf` | `NotoSansJP-Bold.ttf` | `:420-425` | yes |
| `ar` | `IBMPlexSansArabic-Regular.ttf` | `IBMPlexSansArabic-Medium.ttf` | `IBMPlexSansArabic-Bold.ttf` | `:426-431` | yes |
| `fa` | `IBMPlexSansArabic-Regular.ttf` | `IBMPlexSansArabic-Medium.ttf` | `IBMPlexSansArabic-Bold.ttf` | `:432-437` | yes |
| everything else (`ba cs de es fi fr hr hu id it nl no pl pt pt-br ro ru sl sv tr ua`) | Gilroy | Gilroy | Gilroy | `:438-443` | embedded |

Loader (`FxTheme.cpp:691-705`):

```cpp
Typeface::Ptr FxTheme::loadTypeface(String fileName)
{
    MemoryBlock fontBuffer;
    String filePath = File::addTrailingSeparator(File::getCurrentWorkingDirectory().getFullPathName());
    File fontFile = File(filePath+fileName);
    if (fontFile.exists()) { if (fontFile.loadFileAsData(fontBuffer))
        return Typeface::createSystemTypefaceFor(fontBuffer.getData(), fontBuffer.getSize()); }
    return nullptr;
}
```

**Three concrete defects to not reproduce:**

1. The font path is resolved against `getCurrentWorkingDirectory()`, not the executable directory.
   Launch FxSound from any other cwd and every CJK/Thai/Arabic face silently falls back to Gilroy
   (`FxTheme.cpp:445-456` substitutes Gilroy when `loadTypeface` returns `nullptr`), which has no
   CJK/Arabic/Thai coverage → tofu.
2. Cyrillic (`ru`, `ua`), Greek-adjacent and Vietnamese-diacritic coverage rely entirely on Gilroy.
   Gilroy 1.000 ships a Latin/Cyrillic-lite set; `vi` gets Montserrat Alternates precisely because
   Gilroy cannot render Vietnamese tone stacks.
3. `fxsound/Fonts/NotoSansTC-*.otf` can never be loaded — the code asks for `NotoSansTC-*.ttf`
   (`FxTheme.cpp:404-406`). Similarly `fxsound/Fonts/NotoSansArabic-*.ttf` is never loaded (the code
   uses IBM Plex Sans Arabic) and `NotoSans{KR,SC,TC}-Bold.otf` is never loaded (700 maps to Medium).
   **`fxsound/Fonts/` is 58 328 380 B, of which 31 477 740 B (NotoSansTC ×3 + NotoSansArabic ×2 + NotoSans{KR,SC}-Bold) can never be loaded by any code path.**

---

## 5. Internationalised strings

### 5.1 Storage format

The string tables are JUCE `LocalisedStrings` files: a UTF-8 text file, CRLF line endings, an
optional two-line header, then one `"key" = "value"` pair per line.

```
language: Russian                           ← FxSound.ru.txt:1
countries: ru                               ← FxSound.ru.txt:2
                                            ← blank
"Contact us" = "Свяжитесь с нами"
```

* **The key *is* the English source string**, verbatim including punctuation, trailing spaces and
  `\r\n` escapes. In `FxSound.txt` every value equals its key (141/141 identical).
* Escapes present in keys and values: `\'` (escaped apostrophe), `\"` (escaped double quote),
  `\r\n` (literal two-character escape, *not* a real newline), `\\`.
* No BOM. `FxSound.nl.txt` has **no `language:`/`countries:` header at all** — it starts directly at
  the first key.
* `%s` is used for runtime interpolation in exactly 5 keys (§5.5).

Lookup happens through JUCE's `TRANS(...)` macro → `LocalisedStrings::translateWithCurrentMappings`.
**If the key is absent, the key itself is returned.** That is why English needs no mapping at all.

### 5.2 Dispatch — `FxController::setLanguage`

`fxsound/Source/GUI/FxController.cpp:2330-2469`:

```cpp
void FxController::setLanguage(String language_code)
{
    if (language_code.isEmpty()) language_code = "en";       // :2332-2335
    language_ = language_code;
    settings_.setString("language", language_);              // :2338  ← persisted key "language"
    LocalisedStrings::setCurrentMappings(nullptr);           // :2340  ← English = no mapping
    if      (language_.startsWithIgnoreCase("ko"))    …FxSound_ko_txt…      // :2342
    else if (language_.startsWithIgnoreCase("vi"))    …
    …
    else if (language_.startsWithIgnoreCase("cs"))    …FxSound_cs_txt…      // :2454
    theme->loadFont(language_);                              // :2462
    main_window_->sendLookAndFeelChange();                   // :2467
}
```

**Dispatch-order hazard:** `pt-br` is tested *before* `pt` (`:2354` before `:2358`) — correct. But
`startsWithIgnoreCase` means the locale `"no"` would also match nothing else, while a hypothetical
`"nb-NO"` matches nothing at all. There is **no `en` branch**: any code that reaches the `else`
chain's end keeps `nullptr` mappings and renders raw English keys.

Initial value (`FxController.cpp:269-278`):

```cpp
if (language.isEmpty()) {
    language = settings_.getString("language");
    if (language.isEmpty()) language = SystemStats::getDisplayLanguage();
}
setLanguage(language);
```

`--language=<code>` on the command line overrides at runtime (`FxController.cpp:518-521`).
Shutdown clears the mapping: `LocalisedStrings::setCurrentMappings(nullptr);` (`Main.cpp:121`).

### 5.3 Shipped languages

Canonical ordered list, from `fxsound/Source/GUI/FxLanguage.cpp:25` (this is the order the ◀ ▶
stepper in Settings → General cycles through):

```cpp
languages_ = { "en", "ar", "ba", "hr", "cs", "de", "es", "fi", "fr", "hu", "id", "it",
               "ja", "ko", "nl", "no", "fa", "pl", "pt", "pt-br", "ro", "ru", "sl",
               "sv", "th", "tr", "ua", "vi", "zh-CN", "zh-TW" };
```

30 entries. Display names come from `FxController::getLanguageName` (`FxController.cpp:2471-2594`).

| # | Code | `getLanguageName()` | `language:` header | `countries:` header | `BinaryData` symbol | Bytes | Keys present (of 141) |
|---:|---|---|---|---|---|---:|---:|
| 0 | `en` | `English` | *(placeholder)* | *(placeholder)* | `FxSound_txt` (never loaded) | 11 274 | 141 |
| 1 | `ar` | العربية | Arabic | `EG sa` | `FxSound_ar_txt` | 15 263 | 141 (+2 extra) |
| 2 | `ba` | `bosanski` | Bosnian | `ba` | `FxSound_ba_txt` | 11 676 | 141 |
| 3 | `hr` | `hrvatski` | Croatian | `HR` | `FxSound_hr_txt` | 11 525 | 141 |
| 4 | `cs` | `Česky` | `Česky` | `cz` | `FxSound_cs_txt` | 12 136 | 141 |
| 5 | `de` | `Deutsch` | German | `de at ch` | `FxSound_de_txt` | 12 889 | 141 |
| 6 | `es` | `Español` | Spanish | `ar co es mx` | `FxSound_es_txt` | 12 391 | 141 |
| 7 | `fi` | `Suomi` | Finnish | `fi` | `FxSound_fi_txt` | 12 140 | 141 |
| 8 | `fr` | `français` | French | `fr` | `FxSound_fr_txt` | 12 787 | **140** |
| 9 | `hu` | `Magyar` | Hungarian | `hu` | `fxsound_hu_txt` | 13 100 | **140** |
| 10 | `id` | `bahasa Indonesia` | Indonesian | `id` | `FxSound_id_txt` | 11 988 | 141 |
| 11 | `it` | `Italiano` | Italiano | `it` | `FxSound_it_txt` | 12 281 | 141 |
| 12 | `ja` | 日本語 | Japanese | `ja` | `FxSound_ja_txt` | 13 739 | 141 |
| 13 | `ko` | 한국어 | Korean | `kr` | `FxSound_ko_txt` | 12 657 | 141 |
| 14 | `nl` | `Nederlands` | *(none)* | *(none)* | `FxSound_nl_txt` | 11 934 | 141 |
| 15 | `no` | `Norsk` | Norsk | `no` | `FxSound_no_txt` | 12 001 | **129** |
| 16 | `fa` | فارسی | Persian | `ir` | `FxSound_fa_txt` | 15 942 | 141 |
| 17 | `pl` | `Polski` | Polish | `pl` | `FxSound_pl_txt` | 12 699 | 141 |
| 18 | `pt` | `Português` | Portuguese | `pt` | `FxSound_pt_txt` | 12 329 | 141 |
| 19 | `pt-br` | `português brasileiro` | Brazilian Portuguese | `br` | `FxSound_ptbr_txt` | 12 437 | 141 |
| 20 | `ro` | `Română` | Romanian | `ro` | `FxSound_ro_txt` | 12 462 | 141 |
| 21 | `ru` | русский | Russian | `ru` | `FxSound_ru_txt` | 16 615 | 141 |
| 22 | `sl` | `Slovenščina` | Slovenian | `sl` | `FxSound_sl_txt` | 11 762 | **139** |
| 23 | `sv` | `svenska` | Swedish | `se` | `FxSound_sv_txt` | 12 066 | 141 |
| 24 | `th` | แบบไทย | Thai | `th` | `FxSound_th_txt` | 20 011 | 141 |
| 25 | `tr` | `Türk` | Turkish | `tr` | `FxSound_tr_txt` | 12 182 | 141 |
| 26 | `ua` | українська | Ukrainian | `ua` | `FxSound_ua_txt` | 16 213 | **131** |
| 27 | `vi` | `Tiếng Việt` | Vietnamese | `vn` | `FxSound_vi_txt` | 13 351 | 141 |
| 28 | `zh-CN` | 简体中文 | Chinese (Simplified) | `cn sg` | `FxSound_zhCN_txt` | 10 664 | 141 |
| 29 | `zh-TW` | 繁體中文 | Chinese (Traditional) | `Taiwan` | `FxSound_zhTW_txt` | 10 825 | **139** |

`getLanguageName` falls through to `"English"` for anything unrecognised (`FxController.cpp:2593`).
The display names are written as `L"\uXXXX…"` escapes in the source, e.g. `L"한국어"`
for Korean (`FxController.cpp:2479`) and `L"Сlovenščina"`-style for Slovenian
(`FxController.cpp:2582`).

**Non-standard tags to fix in the port:** `ua` → BCP-47 `uk`; `ba` (Bosnian) → `bs` (`ba` is the
country code for Bosnia, not a language); `no` → `nb`; `zh-CN`/`zh-TW` → `zh-Hans`/`zh-Hant`.
Keep the legacy tags as aliases so existing `language=` settings values keep working.

### 5.4 Complete English key table (141 entries)

Extracted verbatim from `BinaryData::FxSound_txt` (11 274 B, `BinaryData.h:296-297`; the
`Resources/Strings/FxSound.txt` source file is absent from this checkout). Escapes shown as they
appear in the file. Ordering is file order; the suggested Rust key is a stable snake_case slug.

| # | Suggested key | English text (= JUCE lookup key) |
|---:|---|---|
| 1 | `err_playback_device_prefix` | `Oops! There\'s an issue with your playback device settings.\r\nBefore we can get started, please go through the ` |
| 2 | `err_troubleshooting_link` | `troubleshooting steps here.` |
| 3 | `err_still_having_problems` | ` if you\'re still having problems.` |
| 4 | `contact_us` | `Contact us` |
| 5 | `err_audio_config_fatal` | `Error in system audio configuration. Unable to run FxSound` |
| 6 | `ok` | `OK` |
| 7 | `whats_new_link` | `Click here to see what\'s new on this version!` |
| 8 | `tray_minimised_hint` | `FxSound in system tray\r\nClick FxSound icon to reopen` |
| 9 | `survey_prompt` | `Thanks for using FxSound! Would you be\r\ninterested in helping us by taking a quick 4 minute\r\nsurvey so we can make FxSound better?` |
| 10 | `survey_link` | `Take the survey.` |
| 11 | `preset_unsaved_exit` | `Changes to your preset are not saved.\r\nDo you want to exit?` |
| 12 | `preset_unsaved_ignore` | `Changes to your preset are not saved.\r\nDo you want to ignore the changes?` |
| 13 | `output_disconnected` | `Output Disconnected` |
| 14 | `output_prefix` | `Output: ` |
| 15 | `preset_changes_saved_fmt` | `Changes to preset %s are saved.` |
| 16 | `preset_new_saved_fmt` | `New preset %s is saved.` |
| 17 | `preset_limit_reached` | `Reached the limit on new presets.` |
| 18 | `preset_deleted_fmt` | `Preset %s is deleted.` |
| 19 | `presets_restored_defaults` | `Presets are restored to factory defaults` |
| 20 | `preset_export_overwrite_fmt` | `Preset file %s already exists in the export path, do you want to overwrite the preset file?` |
| 21 | `fxsound_is_fmt` | `FxSound is %s.` |
| 22 | `on` | `on` |
| 23 | `off` | `off` |
| 24 | `preset_prefix` | `Preset: ` |
| 25 | `effect_clarity` | `Clarity` |
| 26 | `effect_ambience` | `Ambience` |
| 27 | `effect_surround` | `Surround Sound` |
| 28 | `effect_dynamic_boost` | `Dynamic Boost` |
| 29 | `effect_bass_boost` | `Bass Boost` |
| 30 | `tip_clarity` | `Enhances and elevates high end\r\nfidelity and presence` |
| 31 | `tip_ambience` | `Thickens and smooths audio\r\nwith controlled reverberation` |
| 32 | `tip_surround` | `Widens the left-right balance\r\nfor expansive, wide sound` |
| 33 | `tip_dynamic_boost` | `Increases overall volume and balance\r\nwith responsive processing` |
| 34 | `tip_bass_boost` | `Boosts low end for full,\r\nimpactful response` |
| 35 | `tip_eq_band_1` | `Hyper-low Bass - First band for very low frequencies down to 20 Hz.` |
| 36 | `tip_eq_band_2` | `Super-low Bass. Increase this for more rumble and \"thump\", decrease if there\'s too much boominess.` |
| 37 | `tip_eq_band_3` | `Center of your Bass sound. Increase this for a fuller low end, decrease if the bass sounds overwhelming.` |
| 38 | `tip_eq_band_4` | `The low end of your mid-range. Increase this to make vocals sound rich and warm, decrease it to help control instruments that sound loud and muffled.` |
| 39 | `tip_eq_band_5` | `A focal point of the low-mid-range. Increase this to bring out electric guitars and vocal volume, decrease it to reduce any \"boxy\" tones.` |
| 40 | `tip_eq_band_6` | `The center mid-range band. Increase this to drastically boost rhythm instruments and snare hits, reduce it to cut out \"nasal\" tones.` |
| 41 | `tip_eq_band_7` | `The high-mid-range. Increase this to get more instrumental harmonics, reduce it to improve drums that have too much \"clickiness\" or orchestral instruments that are piercing.` |
| 42 | `tip_eq_band_8` | `The lower end of the high-end range. Increase this for more vocal clarity and articulation, reduce it and move the frequency wheel up and down to find and cut out overly loud \"S\" and \"T\" sounds.` |
| 43 | `tip_eq_band_9` | `The core high-end range. Increase this to make your audio sound more like it\'s in an airy, large space, reduce it to help with room noises and unwanted echoing.` |
| 44 | `tip_eq_band_10` | `The highest range of average human hearing. Increase this to give your sound more of a crisp tone, with lots of overtones. Reduce it to remove hiss or painfully high sounds.` |
| 45 | `tip_eq_freq_wheel` | `This wheel allows you to adjust which frequencies this EQ band is affecting\r\nup or down to target different frequencies/pitches. The EQ slider above\r\ncontrols the volume of this EQ band. Increase or decrease to boost or cut\r\na portion of your audio\'s frequencies, without modifying the rest of your sound.` |
| 46 | `subscribe_now` | `SUBSCRIBE NOW` |
| 47 | `yes` | `Yes` |
| 48 | `no` | `No` |
| 49 | `export_presets_title` | `Export Presets` |
| 50 | `export` | `Export` |
| 51 | `export_select_presets` | `Select the presets to export...` |
| 52 | `export_success` | `Presets are exported successfully!` |
| 53 | `import_success` | `Presets successfully imported` |
| 54 | `import_duplicates_skipped` | `Duplicate presets not imported` |
| 55 | `import_presets_title` | `Import Presets` |
| 56 | `import` | `Import` |
| 57 | `import_select_folder` | `Select the folder which contains the presets...` |
| 58 | `folder_label` | `Folder:` |
| 59 | `import_no_presets_found` | `Preset files not found in the selected folder.` |
| 60 | `settings` | `Settings` |
| 61 | `donate` | `Donate` |
| 62 | `tab_general` | `General` |
| 63 | `tab_help` | `Help` |
| 64 | `general_preferences` | `General Preferences` |
| 65 | `launch_on_startup` | `Launch on system startup` |
| 66 | `auto_switch_new_device` | `Automatically switch to newly connected output device` |
| 67 | `hide_help_tips` | `Hide help tips for audio controls` |
| 68 | `disable_hotkeys` | `Disable keyboard shortcuts` |
| 69 | `reset_presets_factory` | `Reset presets to factory defaults` |
| 70 | `hotkey_power` | `Turn FxSound On/Off` |
| 71 | `hotkey_open_close` | `Open/Close FxSound` |
| 72 | `hotkey_next_preset` | `Use Next Preset` |
| 73 | `hotkey_prev_preset` | `Use Previous Preset` |
| 74 | `hotkey_change_device` | `Change Playback Device` |
| 75 | `language` | `Language` |
| 76 | `disable_debug_logging` | `Disable debug logging` |
| 77 | `version` | `Version` |
| 78 | `support` | `Support` |
| 79 | `maintenance` | `Maintenance` |
| 80 | `changelog` | `Changelog` |
| 81 | `quick_tour` | `Quick tour` |
| 82 | `submit_debug_logs` | `Submit debug logs` |
| 83 | `help_center` | `Help center` |
| 84 | `feedback` | `Feedback` |
| 85 | `check_for_updates` | `Check for updates` |
| 86 | `menu_open` | `Open` |
| 87 | `menu_exit` | `Exit` |
| 88 | `menu_turn_off` | `Turn Off` |
| 89 | `menu_turn_on` | `Turn On` |
| 90 | `a11y_preset_select` | `Preset Select` |
| 91 | `a11y_device_select` | `Playback Device Select` |
| 92 | `preset_name_placeholder` | `Enter your preset name` |
| 93 | `preset_new_name_placeholder` | `Enter new preset name` |
| 94 | `preset_overwrite` | `Overwrite Existing Preset` |
| 95 | `preset_save_new` | `Save New Preset` |
| 96 | `preset_undo` | `Undo Preset Changes` |
| 97 | `preset_rename` | `Rename Preset` |
| 98 | `preset_delete` | `Delete Preset` |
| 99 | `preset_download_bonus` | `Download Bonus Presets` |
| 100 | `err_exclusive_mode_prefix` | `FxSound is unable to play processed audio through the selected output device.\r\nAnother application could be using it in exclusive mode or the device could be\r\ndisconnected. To disable exclusive mode follow these ` |
| 101 | `err_steps_link` | `steps.` |
| 102 | `preset_menu_hint` | `Click here to save new presets, overwrite old ones, or reset your settings.` |
| 103 | `err_settings_file_missing` | `Settings file not found!` |
| 104 | `open_source_banner` | `FxSound is now open-source` |
| 105 | `hotkey_capture_hint` | `Press Ctrl + Alt/Shift + 0-9/A-Z to change the hotkey` |
| 106 | `err_mono_device` | `FxSound does not support mono devices, so FxSound processing had been disabled for this device.` |
| 107 | `a11y_minimize_button` | `Minimize Button` |
| 108 | `output_device` | `Output device` |
| 109 | `select_preferred_output` | `Select preferred output` |
| 110 | `preferred_output_label` | `Preferred output:` |
| 111 | `none` | `None` |
| 112 | `newly_connected_device` | `Newly connected output device` |
| 113 | `hide_notifications` | `Hide notifications` |
| 114 | `tab_audio` | `Audio` |
| 115 | `normalize_volume` | `Normalize Volume` |
| 116 | `preset_unsaved_save` | `Changes to your preset are not saved.\r\nDo you want to save?` |
| 117 | `preset_save_title` | `Save Preset` |
| 118 | `save` | `Save` |
| 119 | `cancel` | `Cancel` |
| 120 | `equalizer_label` | `Equalizer:` |
| 121 | `master_gain` | `Master Gain` |
| 122 | `normalization` | `Normalization` |
| 123 | `filter_q` | `Filter Q` |
| 124 | `balance` | `Balance` |
| 125 | `left` | `Left` |
| 126 | `right` | `Right` |
| 127 | `bands_suffix` | ` Bands` |
| 128 | `restore_defaults` | `Restore Defaults` |
| 129 | `automatic_updates` | `Automatic updates` |
| 130 | `always_on_top` | `Always On Top` |
| 131 | `theme` | `Theme` |
| 132 | `theme_dark` | `Dark` |
| 133 | `theme_light` | `Light` |
| 134 | `output_device_preference` | `Output Device Preference` |
| 135 | `select_preset` | `Select preset` |
| 136 | `equalizer` | `Equalizer` |
| 137 | `prioritize_new_devices` | `Prioritize new output devices` |
| 138 | `device_priority_hint` | `Use Shift+Up or Shift+Down to change the device priority` |
| 139 | `err_remote_desktop` | `Audio processing is not available over Remote Desktop` |
| 140 | `volume_leveling` | `Volume Leveling` |
| 141 | `hotkey_not_configured` | `Not configured` |

Leading/trailing spaces in #3, #14, #24, #127 are **significant** — they are concatenated with
runtime values. Do not trim them.

### 5.5 Format strings

Five keys carry a `printf` `%s`: #15, #16, #18, #20, #21. All are `%s` (wide string) only — there
are no `%d`, no positional `%1$s`, no plurals, no gender. Usage example
(`FxSystemTrayView.cpp:76-81`):

```cpp
String param = power ? TRANS(L"on") : TRANS(L"off");
swprintf_s(tool_tip, String(TRANS("FxSound is %s.")).toWideCharPointer(), param.toWideCharPointer());
wcscat_s(tool_tip, 1024, L"\n\n");
wcscat_s(tool_tip, 1024, String(TRANS("Output: ")).toWideCharPointer());
```

> **Security note for the port:** this is an unchecked `swprintf_s` into a fixed 1024-wchar buffer
> driven by a *translator-controlled* format string. In Rust use named placeholders resolved at
> compile time.

### 5.6 Strings referenced by `TRANS()` that are **missing** from every table

Cross-checking the 118 distinct `TRANS("…")` literals in `fxsound/Source/**` against the 141 keys:

| Literal | Site | Consequence |
|---|---|---|
| `Ctrl` | `FxHotkeyLabel.cpp:236` | never translated |
| `Alt` | `FxHotkeyLabel.cpp:240` | never translated |
| `Shift` | `FxHotkeyLabel.cpp:244` | never translated |
| `Power Button` | `FxMainWindow.cpp:194` | a11y help text, never translated |
| `Menu Button` | `FxMainWindow.cpp:200` | a11y, never translated |
| `Resize Button` | `FxMainWindow.cpp:205` | a11y, never translated |
| `Preset List` | `FxView.cpp:32` | a11y description, never translated |
| `Playback Device List` | `FxView.cpp:41` | a11y description, never translated |
| `Audio enhancements are not available over Remote Desktop` | `FxMainWindow.cpp:413` | tooltip; note the table has the *different* string `Audio processing is not available over Remote Desktop` (#139) |
| `FxSound is unable to play processed audio through the selected output device.\n…` | `FxView.cpp:62` | uses `\n`, the table key (#100) uses `\r\n` → **lookup always fails, always English** |

Add all ten to the Rust key table.

### 5.7 Per-language coverage defects

| Language | Missing keys | Cause |
|---|---:|---|
| `no` (Norsk) | **12** | apostrophes/quotes left **unescaped** in the key column (`There's` instead of `There\'s`) → 7 keys never match, plus 5 genuinely absent (`Select the presets to export...`, `Select the folder which contains the presets...`, EQ band tips 5–8) |
| `ua` (Ukrainian) | **10** | same unescaped-apostrophe problem (5 keys) + 5 EQ band tips absent |
| `sl` (Slovenian) | 2 | `Output Device Preference`, `Select preset` absent |
| `zh-TW` | 2 | ` if you\'re still having problems.` present but with the **leading space stripped**; `FxSound is unable to play…` key text differs |
| `fr` | 1 | key mis-typed as `Hyper-low Bass - First band for very basses fréquences down to 20 Hz.` (translator edited the *key*, not the value) |
| `hu` | 1 | key mis-typed as `FxSound cannot play the processed sound through the selected output de…` |
| `ar` | 0 (+2 orphans) | carries extra keys `Change Preset`, `Change/Shift between Preset` that no code references |
| 18 other languages | 0 (+1 orphan `Change Preset`) | — |

`Change Preset` appears as a dead key in 14 languages (`fr id it ja nl pl ptbr ro sv th tr vi hu ar`).
Drop it.

---

## 6. Rust / egui implementation plan

### 6.1 Crate additions

```toml
[dependencies]
eframe        = { version = "0.36", default-features = false, features = ["glow", "wayland", "x11", "default_fonts"] }
egui          = "0.36"
egui_extras   = { version = "0.36", features = ["svg", "image"] }  # optional convenience, see 6.3
resvg         = "0.45"        # pulls usvg + tiny-skia; pin exactly, the API moves between minors
tiny-skia     = "0.11"
image         = { version = "0.25", default-features = false, features = ["png"] }
rust-embed    = { version = "8", features = ["debug-embed"] }      # dev-time hot reload, release-time embed
rust-i18n     = "3"
sys-locale    = "0.3"
once_cell     = "1"
```

Do **not** enable `eframe/wgpu` unless you need it — `glow` keeps the binary smaller and works on
every Mesa driver; the UI is 2-D and never GPU-bound. (`FxSound.jucer` does link `juce_opengl`,
`JuceLibraryCode/include_juce_opengl.cpp`, but only for the spectrum visualiser.)

### 6.2 Asset layout and embedding

```
fxsound-linux/
├── assets/
│   ├── icons/                 # 56 of the 62 live SVGs (chrome), verbatim copies, LF endings
│   │   ├── arrow_down.svg … speaker.svg
│   ├── logo/
│   │   ├── logo-white.svg  logo-black.svg  logo-red.svg  logo-blue.svg
│   │   ├── fxsound-bars-white.svg  fxsound-bars-black.svg
│   ├── app-icon/              # regenerated from the .ico set, PNG 16/22/24/32/48/64/128/256
│   │   ├── fxsound.png  fxsound-active.png  fxsound-processing.png  fxsound-off.png
│   └── fonts/
│       ├── Inter-Regular.ttf  Inter-SemiBold.ttf  Inter-Bold.ttf   # Gilroy replacement, see §7
│       └── (CJK/Thai/Arabic resolved from the system, see 6.5)
└── locales/
    ├── en.yml  ar.yml  bs.yml  cs.yml  de.yml  es.yml  fa.yml  fi.yml  fr.yml  hr.yml
    ├── hu.yml  id.yml  it.yml  ja.yml  ko.yml  nb.yml  nl.yml  pl.yml  pt.yml  pt-BR.yml
    └── ro.yml  ru.yml  sl.yml  sv.yml  th.yml  tr.yml  uk.yml  vi.yml  zh-Hans.yml  zh-Hant.yml
```

Embedding: the whole live SVG set is ~87 KB of text. Use plain `include_bytes!` through a generated
table — it is zero-cost, has no runtime lookup and keeps the `FxImage` indexing that `FxTheme`
already proves works:

```rust
// assets.rs  — mirrors FxTheme.h:32-37 exactly.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
#[repr(usize)]
pub enum Icon {
    DefaultLogo = 0, HighlightedLogo, IconLogo,
    PowerOn, PowerOff, Donate, DonateHover, Menu, MenuHover,
    Minimize, MinimizeHover, Maximize, MaximizeHover, MinWindow, MinWindowHover,
    Flip, FlipHover, RestoreDefaults, RestoreDefaultsHover, Remove,
    ArrowNext, ArrowNextBw, ArrowPrev, ArrowPrevBw,
    ArrowUpAccent, ArrowUp, ArrowDownAccent, ArrowDown, DropDownArrow, DropDownArrowHover,
    SliderThumb, SliderThumbBw,
}
pub const NUM_ICONS: usize = 32;

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum ThemeMode { Dark = 0, Light = 1 }

// [ThemeMode][Icon] -> raw SVG bytes. Transcribed from FxTheme.cpp:32-44.
pub static ICON_SVG: [[&[u8]; NUM_ICONS]; 2] = [
    [ // Dark
        include_bytes!("../assets/logo/logo-white.svg"),
        include_bytes!("../assets/logo/logo-red.svg"),
        include_bytes!("../assets/logo/fxsound-bars-white.svg"),
        include_bytes!("../assets/icons/power_on.svg"),
        include_bytes!("../assets/icons/power_off.svg"),
        include_bytes!("../assets/icons/donate.svg"),
        include_bytes!("../assets/icons/donate_hover.svg"),
        include_bytes!("../assets/icons/menu.svg"),
        include_bytes!("../assets/icons/menu_hover.svg"),
        include_bytes!("../assets/icons/minimize.svg"),
        include_bytes!("../assets/icons/minimize_hover.svg"),
        include_bytes!("../assets/icons/maximize.svg"),
        include_bytes!("../assets/icons/maximize_hover.svg"),
        include_bytes!("../assets/icons/min_window.svg"),
        include_bytes!("../assets/icons/min_window_hover.svg"),
        include_bytes!("../assets/icons/flip_white.svg"),
        include_bytes!("../assets/icons/flip.svg"),
        include_bytes!("../assets/icons/restore_defaults_white.svg"),
        include_bytes!("../assets/icons/restore_defaults.svg"),
        include_bytes!("../assets/icons/remove.svg"),
        include_bytes!("../assets/icons/arrow_next.svg"),
        include_bytes!("../assets/icons/arrow_next_bw.svg"),
        include_bytes!("../assets/icons/arrow_prev.svg"),
        include_bytes!("../assets/icons/arrow_prev_bw.svg"),
        include_bytes!("../assets/icons/arrow_up.svg"),
        include_bytes!("../assets/icons/arrow_up_white.svg"),
        include_bytes!("../assets/icons/arrow_down.svg"),
        include_bytes!("../assets/icons/arrow_down_white.svg"),
        include_bytes!("../assets/icons/dropdown_arrow_bw.svg"),
        include_bytes!("../assets/icons/dropdown_arrow_hover.svg"),
        include_bytes!("../assets/icons/Slider_Thumb.svg"),
        include_bytes!("../assets/icons/Slider_Thumb_bw.svg"),
    ],
    [ // Light — FxTheme.cpp:39-44
        include_bytes!("../assets/logo/logo-black.svg"),
        include_bytes!("../assets/logo/logo-blue.svg"),
        include_bytes!("../assets/logo/fxsound-bars-black.svg"),
        include_bytes!("../assets/icons/power_on_blue.svg"),
        include_bytes!("../assets/icons/power_off_black.svg"),
        include_bytes!("../assets/icons/donate_blue.svg"),
        include_bytes!("../assets/icons/donate_hover_blue.svg"),
        include_bytes!("../assets/icons/menu_black.svg"),
        include_bytes!("../assets/icons/menu_hover_blue.svg"),
        include_bytes!("../assets/icons/minimize_black.svg"),
        include_bytes!("../assets/icons/minimize_hover_blue.svg"),
        include_bytes!("../assets/icons/maximize_black.svg"),
        include_bytes!("../assets/icons/maximize_hover_blue.svg"),
        include_bytes!("../assets/icons/min_window_black.svg"),
        include_bytes!("../assets/icons/min_window_hover_blue.svg"),
        include_bytes!("../assets/icons/flip_black.svg"),
        include_bytes!("../assets/icons/flip_blue.svg"),
        include_bytes!("../assets/icons/restore_defaults_black.svg"),
        include_bytes!("../assets/icons/restore_defaults_blue.svg"),
        include_bytes!("../assets/icons/remove.svg"),
        include_bytes!("../assets/icons/arrow_next_blue.svg"),
        include_bytes!("../assets/icons/arrow_next_bw.svg"),
        include_bytes!("../assets/icons/arrow_prev_blue.svg"),
        include_bytes!("../assets/icons/arrow_prev_bw.svg"),
        include_bytes!("../assets/icons/arrow_up_blue.svg"),
        include_bytes!("../assets/icons/arrow_up_black.svg"),
        include_bytes!("../assets/icons/arrow_down_blue.svg"),
        include_bytes!("../assets/icons/arrow_down_black.svg"),
        include_bytes!("../assets/icons/dropdown_arrow_bw.svg"),
        include_bytes!("../assets/icons/dropdown_arrow_hover_blue.svg"),
        include_bytes!("../assets/icons/Slider_Thumb_blue.svg"),
        include_bytes!("../assets/icons/Slider_Thumb_bw.svg"),
    ],
];

pub fn icon_svg(mode: ThemeMode, icon: Icon) -> &'static [u8] {
    ICON_SVG[mode as usize][icon as usize]
}
```

Use `rust-embed` only for the `locales/` tree (it benefits from `debug-embed=false` hot reload
during translation work) and for the app-icon PNGs.

### 6.3 SVG → `egui::ColorImage` (resvg + tiny-skia)

`egui_extras` ships an `svg` feature that wraps `resvg`, but it renders at the texture's natural
size and gives you no control over supersampling for the filtered slider thumbs. Do it explicitly:

```rust
use std::sync::Arc;
use egui::{ColorImage, TextureHandle, TextureOptions};
use resvg::{tiny_skia, usvg};

/// Rasterise `svg` so that its *content* exactly fills `w_px` × `h_px` device pixels.
/// `w_px`/`h_px` must already include the display scale factor (`ctx.pixels_per_point()`).
pub fn svg_to_color_image(
    svg: &[u8],
    w_px: u32,
    h_px: u32,
    opt: &usvg::Options<'_>,
) -> Result<ColorImage, usvg::Error> {
    let tree = usvg::Tree::from_data(svg, opt)?;

    let src = tree.size();                       // intrinsic viewBox size, e.g. 6.0 × 5.0
    let sx = w_px as f32 / src.width();
    let sy = h_px as f32 / src.height();

    let mut pixmap = tiny_skia::Pixmap::new(w_px, h_px)
        .expect("non-zero pixmap dimensions");

    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(sx, sy),
        &mut pixmap.as_mut(),
    );

    // tiny-skia stores PREMULTIPLIED RGBA8. egui wants straight (un-premultiplied) alpha
    // in ColorImage, so demultiply per pixel — skipping this is the classic
    // "dark halo around every icon" bug.
    let mut pixels = Vec::with_capacity((w_px * h_px) as usize);
    for p in pixmap.pixels() {
        let c = p.demultiply();
        pixels.push(egui::Color32::from_rgba_unmultiplied(
            c.red(), c.green(), c.blue(), c.alpha(),
        ));
    }

    // `from_rgba_unmultiplied` also sets `source_size`; constructing the struct literally
    // breaks between egui minors, so prefer a constructor where possible.
    Ok(ColorImage {
        size: [w_px as usize, h_px as usize],
        pixels,
        source_size: egui::vec2(src.width(), src.height()),
    })
}

/// Aspect-preserving variant, matching JUCE `Drawable::drawWithin(..., RectanglePlacement::centred)`.
/// Used for slots whose box aspect differs from the viewBox — e.g. `IconLogo` (299.83×219.26 into 19×14).
pub fn svg_to_color_image_fitted(
    svg: &[u8], box_w: u32, box_h: u32, opt: &usvg::Options<'_>,
) -> Result<ColorImage, usvg::Error> {
    let tree = usvg::Tree::from_data(svg, opt)?;
    let src = tree.size();
    let s = (box_w as f32 / src.width()).min(box_h as f32 / src.height());
    let (w, h) = ((src.width() * s).round() as u32, (src.height() * s).round() as u32);
    svg_to_color_image(svg, w.max(1), h.max(1), opt)
}
```

`usvg::Options` setup — the SVGs here use no `<text>`, so an empty `fontdb` is fine and saves a
fontconfig scan:

```rust
static USVG_OPTS: once_cell::sync::Lazy<usvg::Options<'static>> =
    once_cell::sync::Lazy::new(|| {
        let mut o = usvg::Options::default();
        o.shape_rendering = usvg::ShapeRendering::GeometricPrecision;
        o.image_rendering = usvg::ImageRendering::OptimizeQuality;
        // No <text> in any FxSound asset — leave fontdb empty (Arc::new(fontdb::Database::new())).
        o
    });
```

#### Texture cache

Key on `(icon, theme, w_px, h_px)` so a DPI change or a Lite↔Pro resize re-rasterises instead of
scaling a stale bitmap:

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct IconKey { icon: Icon, mode: ThemeMode, w: u32, h: u32 }

#[derive(Default)]
pub struct IconCache(std::collections::HashMap<IconKey, TextureHandle>);

impl IconCache {
    pub fn get(&mut self, ctx: &egui::Context, icon: Icon, mode: ThemeMode, pts: egui::Vec2)
        -> TextureHandle
    {
        let ppp = ctx.pixels_per_point();
        let key = IconKey { icon, mode,
            w: (pts.x * ppp).round().max(1.0) as u32,
            h: (pts.y * ppp).round().max(1.0) as u32 };
        self.0.entry(key).or_insert_with(|| {
            let img = svg_to_color_image(icon_svg(mode, icon), key.w, key.h, &USVG_OPTS)
                .expect("bundled SVG must parse");
            ctx.load_texture(
                format!("{icon:?}-{mode:?}-{}x{}", key.w, key.h),
                img,
                TextureOptions::LINEAR,   // NEAREST for the 6×5 arrows if they look mushy
            )
        }).clone()
    }
}
```

Then draw at the exact point sizes from §3.2:

```rust
let tex = cache.get(ctx, Icon::PowerOn, mode, egui::vec2(24.0, 24.0));       // FxMainWindow.h:54
ui.add(egui::Image::new(&tex).fit_to_exact_size(egui::vec2(24.0, 24.0)));
```

For the two 64×64 filtered thumbs, rasterise at `2 ×` the logical size and let egui downsample —
`feGaussianBlur` at 16 px looks noticeably banded otherwise.

Total rasterisation cost at startup for the Dark set at 1× is well under 5 ms; do it lazily anyway.

#### App / tray icons

`eframe::NativeOptions { viewport: ViewportBuilder::default().with_icon(...) }` wants an
`egui::IconData { rgba, width, height }`. Decode the 256×256 PNG with the `image` crate, or render
`logo-*.svg` at 256×256 through the same path. Ship the four state icons as a freedesktop icon theme
(`$XDG_DATA_HOME/icons/hicolor/<size>/apps/fxsound.png`) so a StatusNotifierItem host can pick the
right size — see §7.4.

### 6.4 Fonts in `egui::FontDefinitions`

JUCE's `Typeface::createSystemTypefaceFor` maps to inserting a `FontData` and naming it; JUCE's
single `setDefaultSansSerifTypeface(font_600_)` maps to putting the Semibold face first in
`FontFamily::Proportional`.

Model the three JUCE weight slots as three named families so the 17/14 px call sites transfer 1:1:

```rust
use std::sync::Arc;
use egui::{FontData, FontDefinitions, FontFamily, FontTweak};

pub const UI_400: &str = "ui-400";   // FxTheme::getSmallFont()  → 14 px
pub const UI_600: &str = "ui-600";   // FxTheme::getNormalFont() → 17 px  (also the default)
pub const UI_700: &str = "ui-700";   // FxTheme::getTitleFont()  → 17 px

pub fn build_fonts(lang: &LangTag, sys: &SystemFonts) -> FontDefinitions {
    let mut f = FontDefinitions::empty();

    // ---- Latin core (Gilroy replacement — see §7 on licensing) ----
    f.font_data.insert("latin-400".into(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf"))));
    f.font_data.insert("latin-600".into(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-SemiBold.ttf"))));
    f.font_data.insert("latin-700".into(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter-Bold.ttf"))));

    // ---- Script fallbacks, loaded from the system (see 6.5) ----
    for (name, bytes) in sys.fallbacks_for(lang) {
        f.font_data.insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
    }

    // Fallback CHAINS. egui walks the Vec left→right per codepoint, so the
    // script font must come AFTER the Latin face or it steals Latin glyphs.
    let chain = |weight: &str| -> Vec<String> {
        let mut v = vec![format!("latin-{weight}")];
        v.extend(sys.chain_names_for(lang, weight));   // e.g. ["cjk-sc-600", "arabic-600", "thai-600", "emoji"]
        v
    };

    f.families.insert(FontFamily::Name(UI_400.into()), chain("400"));
    f.families.insert(FontFamily::Name(UI_600.into()), chain("600"));
    f.families.insert(FontFamily::Name(UI_700.into()), chain("700"));

    // egui's built-ins still need to resolve.
    f.families.insert(FontFamily::Proportional, chain("600"));  // FxTheme.cpp:458 — Semibold IS the default
    f.families.insert(FontFamily::Monospace, vec!["latin-400".into()]);

    f
}
```

Apply with `ctx.set_fonts(build_fonts(...))` inside the language-change handler — the exact analogue
of `FxTheme::loadFont(language_)` + `sendLookAndFeelChange()` (`FxController.cpp:2462-2468`).
`set_fonts` invalidates the glyph atlas; do it once per language switch, never per frame.

Then the text styles:

```rust
use egui::{FontId, TextStyle};
let mut style = (*ctx.style()).clone();
style.text_styles = [
    (TextStyle::Body,    FontId::new(17.0, FontFamily::Name(UI_600.into()))),  // FxTheme.cpp:468
    (TextStyle::Button,  FontId::new(17.0, FontFamily::Name(UI_600.into()))),  // FxTheme.cpp:463
    (TextStyle::Small,   FontId::new(14.0, FontFamily::Name(UI_400.into()))),  // FxTheme.cpp:473
    (TextStyle::Heading, FontId::new(17.0, FontFamily::Name(UI_700.into()))),  // FxTheme.cpp:478
    (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
].into();
ctx.set_style(style);
```

`FontTweak` is the knob for matching Gilroy's optical metrics when substituting: Gilroy sits high on
the baseline relative to Inter/Noto. Start from
`FontTweak { scale: 1.0, y_offset_factor: 0.0, y_offset: 0.0, baseline_offset_factor: 0.0 }` and
tune `y_offset_factor` in ±0.01 steps against a screenshot diff of the original 57 px title bar.
CJK faces normally need `scale: 0.95..1.0` so 17 px Han doesn't overflow the 20 px combo boxes
(`FxAudioControls.h:99` `COMBOBOX_HEIGHT = 20`).

### 6.5 Fallback chains per language

Reproduce the §4.4 mapping, but source the script faces from the system instead of embedding 80 MB:

| Language(s) | Chain after the Latin face | Linux package that provides it |
|---|---|---|
| `en` and all Latin/Cyrillic locales (`bs cs de es fi fr hr hu id it nb nl pl pt pt-BR ro ru sl sv tr uk`) | *(Latin face must cover Latin-Ext + Cyrillic)* | bundled |
| `vi` | Latin face must cover Vietnamese tone stacks — Inter does; Gilroy did not, hence Montserrat Alternates upstream | bundled |
| `ja` | `Noto Sans JP` → `Noto Sans CJK JP` | `noto-fonts-cjk` |
| `ko` | `Noto Sans KR` → `Noto Sans CJK KR` | `noto-fonts-cjk` |
| `zh-Hans` | `Noto Sans SC` → `Noto Sans CJK SC` | `noto-fonts-cjk` |
| `zh-Hant` | `Noto Sans TC` → `Noto Sans CJK TC` | `noto-fonts-cjk` |
| `th` | `Noto Sans Thai` | `noto-fonts` |
| `ar`, `fa` | `IBM Plex Sans Arabic` → `Noto Sans Arabic` → `Noto Naskh Arabic` | `ttf-ibm-plex` / `noto-fonts` |

Resolve with `fontdb` (already a `resvg` dependency, so it costs nothing extra):

```rust
use fontdb::{Database, Family, Query, Source, Stretch, Style, Weight};

fn load_family(db: &Database, family: &str, weight: Weight) -> Option<Vec<u8>> {
    let id = db.query(&Query {
        families: &[Family::Name(family)],
        weight, stretch: Stretch::Normal, style: Style::Normal,
    })?;
    db.with_face_data(id, |data, _idx| data.to_vec())
    // NOTE: `_idx` is the face index inside a TTC. Noto Sans CJK ships as
    // NotoSansCJK-Regular.ttc — egui's FontData carries an `index` field; set it,
    // or the wrong CJK language variant renders (SC glyphs in a JP UI).
}
```

Ship a graceful degradation path: if the chain for the selected language resolves to nothing, fall
back to English and show a one-line warning, rather than rendering a screen of tofu (which is what
upstream does when cwd is wrong — §4.4 defect 1).

### 6.6 i18n crate and file layout

The source format (`"english" = "translation"`, no plurals, no genders, 5 `%s`) is a poor fit for
Fluent's machinery. Use **`rust-i18n` 3.x**: compile-time embedded YAML, `t!("key")`, named
interpolation, runtime locale switching — which is exactly what `setLanguage` does.

`locales/en.yml`:

```yaml
_version: 2
en:
  ok: "OK"
  cancel: "Cancel"
  on: "on"
  off: "off"
  output_prefix: "Output: "
  fxsound_is_fmt: "FxSound is %{state}."
  preset_changes_saved_fmt: "Changes to preset %{name} are saved."
  preset_new_saved_fmt: "New preset %{name} is saved."
  preset_deleted_fmt: "Preset %{name} is deleted."
  preset_export_overwrite_fmt: "Preset file %{name} already exists in the export path, do you want to overwrite the preset file?"
  tip_eq_freq_wheel: |-
    This wheel allows you to adjust which frequencies this EQ band is affecting
    up or down to target different frequencies/pitches. The EQ slider above
    controls the volume of this EQ band. Increase or decrease to boost or cut
    a portion of your audio's frequencies, without modifying the rest of your sound.
```

Rules for the conversion pass:

1. Key = the snake_case slug from the §5.4 table. **Never key on English text** — that is the root
   cause of six of the seven coverage defects in §5.7.
2. `\r\n` inside a JUCE value becomes a real `\n` in YAML (use `|-` block scalars). The Windows
   `\r` was only there because JUCE rendered it literally.
3. `\'` → `'`, `\"` → `"`.
4. `%s` → a *named* placeholder: `%{state}`, `%{name}`. Order is unambiguous because every string
   has at most one.
5. Preserve leading/trailing spaces with explicit quoting (`"Output: "`, `" Bands"`,
   `" if you're still having problems."`, `"Preset: "`).
6. Add the 10 never-translated strings from §5.6 and drop the dead `Change Preset` / `Change/Shift
   between Preset` keys.
7. Rename files to BCP-47: `ua.txt → uk.yml`, `ba.txt → bs.yml`, `no.txt → nb.yml`,
   `zh-CN → zh-Hans`, `zh-TW → zh-Hant`, `pt-br → pt-BR`.

Wire-up:

```rust
rust_i18n::i18n!("locales", fallback = "en");

pub fn set_language(tag: &str) {
    let tag = normalise(tag);                 // "ua"→"uk", "no"→"nb", "zh-CN"→"zh-Hans", …
    rust_i18n::set_locale(&tag);
    settings.set_string("language", &tag);    // same settings key as FxController.cpp:2338
    ctx.set_fonts(build_fonts(&tag, &sys_fonts));   // ← FxTheme::loadFont equivalent
    ctx.request_repaint();                    // ← sendLookAndFeelChange equivalent
}

// startup, mirroring FxController.cpp:269-278
let lang = cli_language
    .or_else(|| settings.get_string("language"))
    .or_else(|| sys_locale::get_locale())     // ← SystemStats::getDisplayLanguage()
    .unwrap_or_else(|| "en".to_string());
set_language(&lang);
```

`sys_locale::get_locale()` reads `LC_ALL` → `LC_MESSAGES` → `LANG` and returns e.g. `"ru_RU.UTF-8"`;
normalise to `"ru"` before lookup. On a GNOME/KDE Wayland session also consider
`org.freedesktop.portal.Settings` / `org.gnome.system.locale` if you want live locale changes, but
the env var is sufficient and matches upstream behaviour.

The language stepper widget from `FxLanguage.cpp` maps to a simple `◀ [name] ▶` row: 180×30 total,
14×22 buttons inset 10 px from each edge, 22 px label between them, background = `ControlBackground`
with a 5 px corner radius (`FxLanguage.h:29-30, 36-38`, `FxLanguage.cpp:42-46`, `:78-82`).
Prefer a plain `egui::ComboBox` on Linux — a 30-entry stepper is hostile — but keep the stepper's
exact ordering (§5.3) as the combo's item order if you want visual parity.

---

## 7. Licensing

The repository is **AGPL-3.0** (`LICENSE`, 661 lines, "GNU AFFERO GENERAL PUBLIC LICENSE Version 3,
19 November 2007"). Every `.cpp`/`.h` under `fxsound/Source/` carries the AGPL header, e.g.
`FxTheme.cpp:1-17`. **The AGPL covers the code. It does not and cannot relicense third-party
assets shipped alongside it.**

| Asset family | Actual licence | `fsType` | A Linux fork must… |
|---|---|---|---|
| **Gilroy** (`Gilroy-{Regular,Semibold,Bold}.ttf`) | **Proprietary.** `name` ID 0 = "Copyright © 2016 by Radomir Tinkov. All rights reserved.", version 1.000. **No ID 13 (licence) and no ID 14 (licence URL) — it is not an open-licence font.** `OS/2.fsType = 0x0004` = *Preview & Print embedding only*: the font may be embedded in a document for viewing/printing but **may not be permanently installed on the receiving system, and must not be redistributed as a font file**. | `0x0004` | **Replace.** Redistributing `Gilroy-*.ttf` inside an AGPL source tree is already questionable upstream; a fork must not carry it. Closest free geometric-grotesque substitutes, in order of preference: **Inter** (SIL OFL 1.1 — has 400/600/700, excellent Cyrillic + Vietnamese, hinted for small sizes), **Manrope** (OFL, closer to Gilroy's roundness), **Poppins** (OFL, geometric but wider). Budget a metrics pass (§6.4 `FontTweak`) — Inter's x-height is larger than Gilroy's, so 17 px Inter reads bigger than 17 px Gilroy. |
| **Noto Sans KR / SC / TC / JP** | SIL OFL 1.1. Copyright "© 2014-2020 Adobe (http://www.adobe.com/)" / "(c) 2014-2021 Adobe …, with Reserved Font Name 'Source'". Trademark "Noto is a trademark of Google Inc." | `0x0000` | **Keep or, better, don't bundle.** OFL redistribution is fine, but 34 MB of CJK per architecture is absurd on Linux where `noto-fonts-cjk` is a one-line dependency. Resolve via `fontdb` (§6.5) and declare a package dependency. |
| **Noto Sans Thai** | SIL OFL 1.1. "Copyright 2022 The Noto Project Authors (https://github.com/notofonts/thai)" | `0x0000` | Same — depend on `noto-fonts`. |
| **Noto Sans Arabic** (in `fxsound/Fonts/`, dead) | SIL OFL 1.1. "Copyright 2015-2020 Google LLC." | `0x0000` | Delete — the code never loads it. |
| **IBM Plex Sans Arabic** | SIL OFL 1.1. "Copyright 2019 IBM Corp. All rights reserved.", version 1.1 | `0x0000` | Keep as the `ar`/`fa` preference; depend on `ttf-ibm-plex` or bundle (700 KB for three weights is acceptable). |
| **Montserrat Alternates** | SIL OFL 1.1. "Copyright 2011 The Montserrat Project Authors", version 7.200 | `0x0000` | Only needed for `vi`; if the Latin replacement covers Vietnamese (Inter does), drop it entirely. |
| **The 69 SVGs + 6 PNGs + 5 ICOs** | No per-file licence header. Author-owned FxSound LLC artwork, inherited AGPL-3.0 by virtue of being in the repo, **but they are also FxSound's trade dress.** | — | **Keep the geometry, rename the brand.** A fork may reuse the AGPL'd SVG sources, but `logo-*.svg` / `FxSound * Bars.svg` / the `.ico` set are the FxSound wordmark and logotype. Trademark law is independent of copyright: ship a distinct name and mark unless the project is an official downstream. The generic chrome (`arrow_*`, `menu*`, `minimize*`, `maximize*`, `min_window*`, `flip*`, `restore_defaults*`, `remove`, `dropdown_arrow*`, `Slider_Thumb*`, `speaker`, `settings`, `question`) carries no brand and can be reused verbatim. |
| **`Resources/Strings/*.txt` (30 files, in `BinaryData.cpp`)** | AGPL-3.0 with the rest of the tree. Community-translated. | — | Keep. Carry the translator credit forward if you can recover it from git history — most files are community contributions. |
| **`bin/BonusPresets/*.fac`** incl. the 8 "(Quizal)" presets and `MeaningfulPresets` | AGPL-3.0 by inclusion; several are clearly third-party community submissions | — | Keep; attribute the `(Quizal)` set. |
| **`Installer/**` binaries** (`DfxSetupDrv.exe`, `ptdevcon*.exe`, `dfxui_*.exe`, `*.dll`) | Windows-only, unsigned-source binaries | — | **Delete.** No Linux use, and shipping prebuilt Windows binaries in a Linux fork is a supply-chain liability. |

### 7.1 Concrete licensing action list for the fork

1. `git rm fxsound/Fonts/Gilroy-*.ttf`; add `assets/fonts/Inter-{Regular,SemiBold,Bold}.ttf` + a
   copy of `OFL.txt`.
2. Drop all bundled Noto; add runtime dependencies `noto-fonts`, `noto-fonts-cjk` in the packaging
   metadata.
3. Add `assets/fonts/LICENSES.md` listing every remaining face, its OFL copy and its copyright line.
4. Replace the wordmark: the five 526.19×75.15 `logo-*.svg` files and the two 299.83×219.26
   `FxSound * Bars.svg` files, plus all five `.ico` files.
5. Delete `button-{normal,hover,depressed}.svg` (873 737 B of unused Sketch export),
   `Button_{ON,OFF}.svg`, `FxSound Logo.svg` (duplicate), `fxsound.png`, `fxsound_large.png`,
   `equalizer.svg`, `logo-{red,white}.png`, `FxSound Logo White.png`, `FxSound White Bars.png`.

---

## 8. Windows-specific machinery in this subsystem → Linux equivalents

| Windows mechanism | What it achieves | Linux / Wayland / PipeWire replacement |
|---|---|---|
| `BinaryData.cpp` (Projucer blob) | Assets linked into the `.exe` | `include_bytes!` / `rust-embed`. There is no Projucer step; assets stay as files in the repo and the compiler does the embedding. |
| `Typeface::createSystemTypefaceFor(ptr, size)` | Register an in-memory font | `FontDefinitions::font_data.insert(name, Arc::new(FontData::from_static(...)))` |
| `loadTypeface()` reading from `getCurrentWorkingDirectory()` (`FxTheme.cpp:694`) | Load a font shipped next to the exe | **Never use cwd.** Resolve via `fontdb`/fontconfig by family name (§6.5); if you must ship files, find them relative to `std::env::current_exe()` or under `$XDG_DATA_DIRS/fxsound/fonts`. |
| `.rc` `ICON DISCARDABLE` + `LoadIcon(hInst, L"IDI_LOGO_RED")` (`FxSystemTrayView.cpp:96`) | Per-state window/tray icon from PE resources | `ViewportBuilder::with_icon(IconData{..})` for the window; for the tray, register icon **names** with a StatusNotifierItem implementation (`ksni` crate) and install PNGs into `hicolor` — SNI hosts look icons up by name, not by handle. |
| `Shell_NotifyIcon` + `NOTIFYICONDATA` + `trayIconGuid_` (`FxSystemTrayView.cpp:24-25`, `:86-120`) | System tray icon + tooltip | `org.kde.StatusNotifierItem` over D-Bus via the `ksni` crate. **There is no tray on a bare Wayland compositor** — GNOME needs the AppIndicator extension. Provide a no-tray fallback (a `--no-tray` mode plus a `.desktop` launcher) rather than assuming it exists. |
| `Shell_NotifyIconGetRect` (`FxSystemTrayView.cpp:136`) to position the toast next to the tray icon | Anchor a popup to the tray icon | **Impossible on Wayland** — a client cannot learn its own or another surface's global position. Replace `FxNotification` (216×80, max 560×120, `FxNotification.h:33-36`) with `org.freedesktop.Notifications` desktop notifications (the `notify-rust` crate), passing `app_icon` = your installed icon name. The 79×12 logo inside the toast then disappears — that's correct, the notification daemon owns the presentation. |
| `SystemStats::getDisplayLanguage()` (`FxController.cpp:274`) | User's UI language from Windows MUI | `sys_locale::get_locale()` (`LC_ALL`/`LC_MESSAGES`/`LANG`), normalised to a BCP-47 tag. |
| `swprintf_s(tool_tip, 1024, fmt, …)` (`FxSystemTrayView.cpp:79`) | Format the tray tooltip | `rust_i18n::t!("fxsound_is_fmt", state = …)` — no fixed buffer, no translator-controlled format string. |
| `MOD_CONTROL` / `MOD_ALT` / `MOD_SHIFT` label strings (`FxHotkeyLabel.cpp:234-245`) | Render the registered `RegisterHotKey` combo | Global hotkeys are **not available to a Wayland client**. Delegate: expose the five actions (`Turn FxSound On/Off`, `Open/Close FxSound`, `Use Next Preset`, `Use Previous Preset`, `Change Playback Device` — keys #70–74) over D-Bus / MPRIS and let the compositor bind them, or use `org.freedesktop.portal.GlobalShortcuts` where available. The hotkey *labels* then come from the compositor, so keys #105 (`Press Ctrl + Alt/Shift + 0-9/A-Z…`) and #141 (`Not configured`) plus `Ctrl`/`Alt`/`Shift` from §5.6 need rewording, not translating. |
| `LocalisedStrings::setCurrentMappings` + `TRANS()` key-is-English-text | Runtime string lookup with identity fallback | `rust_i18n::set_locale` + `t!("slug")` with `fallback = "en"`. The identity-fallback behaviour is what makes the English table redundant upstream; in Rust the `en.yml` file becomes mandatory. |
| `Drawable::createFromImageData` (JUCE's own SVG subset) | Parse an SVG string into a vector drawable | `usvg::Tree::from_data` + `resvg::render` — a strictly larger SVG subset, including the filter chains in `Slider_Thumb*.svg` that JUCE approximates. |
| `RectanglePlacement::{xMid,yMid,doNotResize,stretchToFit,centred}` | JUCE's fit modes | `egui::Image::fit_to_exact_size` (= `stretchToFit`), `.max_size` + `.maintain_aspect_ratio(true)` (= `centred`), or rasterise at the exact target and place with `Ui::put` (= `doNotResize`). §6.3's `svg_to_color_image_fitted` covers the `centred` case. |

---

## 9. Open questions / risks for the Rust port

1. **Complex-script shaping is the single biggest blocker.** egui/epaint 0.36 lays out text with
   `ab_glyph` and does **no** HarfBuzz-class shaping, no BiDi reordering and no mark positioning.
   That means `ar` and `fa` (keys #1–#141 in Arabic/Persian) will render as isolated,
   left-to-right, unjoined letterforms, and `th` will stack combining marks at the wrong advances.
   `ja`/`ko`/`zh-*` are safe (no shaping needed). Options, none free:
   (a) ship `ar`/`fa`/`th` behind a `--enable-experimental-locales` flag and default them to English;
   (b) pre-shape those languages' strings with `rustybuzz` + `unicode-bidi` at load time and emit
   `epaint::Shape::Mesh` from a `swash`-rasterised atlas for every RTL/complex label — roughly a
   1500-line custom text layer;
   (c) wait for / contribute upstream `cosmic-text` integration in epaint.
   **Decide this before writing any UI code** — option (b) changes how every label is drawn.
   Upstream ships `ar`, `fa`, `th` with real translations (15 263 / 15 942 / 20 011 B), so dropping
   them is a visible regression.
2. **Gilroy replacement changes every layout number in `docs/spec/`.** The 57 px title bar, 20 px
   combo boxes, 18 px sliders, 106×15 logo box and 52 px label columns (`FxAudioControls.h:96`) were
   all eyeballed against Gilroy at 14/17 px. Inter at the same nominal size is optically ~4 % larger.
   Either tune `FontTweak::scale` down to ~0.96 or re-derive the spacing constants — do not do both.
3. **Noto Sans CJK TTC face index.** If you resolve CJK through the distro's
   `NotoSansCJK-Regular.ttc`, `FontData` must carry the right `index` (0=JP, 1=KR, 2=SC, 3=TC in the
   common Google build, but distros repack). Getting it wrong renders Simplified glyph forms in a
   Japanese UI — subtle and embarrassing. Prefer the per-language `.otf` subsets where the distro
   ships them.
4. **`Slider_Thumb.svg` filter fidelity.** resvg renders the drop-shadow/inner-shadow chain
   *correctly*, which means the thumb will look different from (better than) the shipped Windows
   build, where JUCE approximates filters. If pixel-parity with Windows is a goal, you need to
   flatten these two SVGs to pre-rendered PNGs from a Windows screenshot instead. Recommendation:
   accept the improvement.
5. **`Resources/Strings/` is empty in this checkout.** All 30 tables were recovered from
   `BinaryData.cpp` (verified: all 100 resources round-trip byte-exactly against the `…Size`
   constants in `BinaryData.h`). If upstream later restores the directory, re-diff — the blob was
   generated at v1.2.15.0 and may lag the sources.
6. **Six languages have broken keys** (§5.7). `no` loses 12 strings and `ua` loses 10 purely to
   unescaped apostrophes. The conversion script must be written against the *value* column and
   re-key from English by position, not by matching the key column, or you will faithfully port the
   bugs.
7. **`FxView.cpp:62` uses `\n` where the table key uses `\r\n`** → the exclusive-mode error message
   (a 200-character, user-facing, frequently-hit error) is untranslated in all 29 languages. Fix in
   the port and the string suddenly appears in 29 locales that have never had it reviewed in
   context — expect layout overflow in `de`, `ru`, `fi`.
8. **Tray + tray-anchored toast.** `FxNotification` is anchored to the tray icon's screen rect
   (`FxSystemTrayView.cpp:123-140`). Neither the rect query nor client-side global positioning
   exists on Wayland. Moving to `org.freedesktop.Notifications` loses the custom 216×80 / 560×120
   card, the embedded 79×12 logo, the 7000 ms auto-hide (`FxNotification.cpp:67`) and the inline
   hyperlink (keys #2, #10, #101). Confirm with the product owner that desktop notifications are
   acceptable before building anything else.
9. **Global hotkeys.** Five user-configurable combos exist upstream
   (`Installer/Resources/FxSound.settings` ships defaults `cmd_on_off=393297`,
   `cmd_open_close=393285`, `cmd_next_preset=393281`, `cmd_previous_preset=393306`,
   `cmd_change_output=393303`). A Wayland client cannot grab these. The whole Settings→General
   hotkey section (keys #68, #70–#74, #105, #141 + `Ctrl`/`Alt`/`Shift`) needs redesign, not
   translation.
10. **Icon rasterisation on HiDPI / fractional scaling.** The cache key in §6.3 includes device
    pixels, which is correct, but Wayland fractional scaling (`wp_fractional_scale_v1`) can hand you
    `pixels_per_point()` = 1.25 / 1.5 / 1.75. The 6×5 arrows become 7.5×6.25 px — round up and
    accept a half-pixel offset, or switch those four tiny assets to hand-drawn `egui::Shape`
    triangles, which will look sharper at every scale.
11. **`blue_logo.ico` has only a 256×256 frame** while the other three state icons have
    16/24/32/48/256. Whatever you generate for the Linux icon theme must fill all sizes for all four
    states or the Light-theme "processing" tray icon will be visibly softer than the others.
12. **Theme switching cost.** `FxTheme::setThemeMode` re-runs `init()` (`FxTheme.cpp:491-498`),
    which re-parses four SVGs. The Rust cache keys on `ThemeMode`, so a toggle re-rasterises up to
    32 icons — measure it, and pre-warm both themes at startup if it exceeds one frame budget.
    Also decide whether to follow the desktop's light/dark preference
    (`org.freedesktop.appearance color-scheme` via the Settings portal) in addition to the app's own
    Dark/Light setting (keys #131–#133).
13. **`equalizer.svg` is marked `resource="0"`** in both `.jucer` files — it was deliberately
    excluded. It may be a planned-but-unshipped feature icon. Don't resurrect it without asking.
