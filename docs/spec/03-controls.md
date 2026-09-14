# 03 — Audio Effect Controls

Reverse-engineering spec for the FxSound **audio effect control** subsystem, written so that it can
be rebuilt from scratch in Rust 1.98.1 with `egui`/`eframe` 0.36.0 (winit / native Wayland, wgpu or
glow) on top of PipeWire.

All paths in citations are relative to the repository root
`/home/blackixxce/Загрузки/fxsound-app-main/`. Every literal number below is either read directly
out of a cited line, or explicitly marked **(derived)** with the arithmetic shown.

Primary sources read in full for this document:

| File | Lines | Role |
|---|---:|---|
| `fxsound/Source/GUI/FxAudioControls.h` | 165 | Panel + effect-slider + EQ-control class definitions, all geometry constants |
| `fxsound/Source/GUI/FxAudioControls.cpp` | 551 | Panel layout, the five effect sliders, the four level sliders, flip/restore buttons |
| `fxsound/Source/GUI/FxAudioSlider.h/.cpp` | 47 / 100 | Generic "value-label follows the thumb" slider (Master Gain, Volume Leveling, Filter Q) |
| `fxsound/Source/GUI/FxBalanceSlider.h/.cpp` | 52 / 156 | Balance slider with its own two-sided gradient track painter |
| `fxsound/Source/GUI/FxPowerButton.h/.cpp` | 50 / 87 | Two-state power toggle in the title bar |
| `fxsound/Source/GUI/FxComboBox.h/.cpp` | 44 / 82 | Preset / playback-device / EQ-band drop-down |
| `fxsound/Source/GUI/FxHotkeyLabel.h/.cpp` | 58 / 256 | Hotkey name + inline hotkey capture editor |
| `fxsound/Source/GUI/FxHyperlink.h/.cpp` | 39 / 49 | Underlined text link |
| `fxsound/Source/GUI/FxPresetNameEditor.h/.cpp` | 62 / 127 | Preset-name text field + filename-safe input filter |

Supporting files read for the numbers they supply: `FxTheme.h/.cpp` (palette, fonts, slider and
combo painting), `FxController.h/.cpp` (defaults, DSP plumbing, hotkeys, preset lifecycle),
`FxModel.h/.cpp` (preset list + modified flag), `FxView.cpp` (combo population), `FxProView.cpp`,
`FxLiteView.cpp`, `FxMainWindow.cpp`, `FxWindow.cpp`, `FxSettingsDialog.cpp/.h`,
`fxsound/Source/Utils/Settings/Settings.cpp` (default hotkey codes), `dsp/include/DfxDsp.h`,
`dsp/DfxDspPrivate.cpp`, and the SVG assets under `fxsound/Images/`.

---

## 1. Subsystem overview

The effect controls live in one 168 × 257 px rounded panel, `FxAudioControls`
(`FxAudioControls.h:147-148`). The panel is a **two-sided card**: it shows *either* the five effect
sliders (`FxEffects`) *or* the equalizer/levels controls (`FxEqualizerControl`), never both. A small
flip button in the panel's top-right corner swaps the faces
(`FxAudioControls.cpp:41-46`, `FxAudioControls.cpp:83`).

```
                     FxAudioControls  (168 x 257, corner radius 8)
   +--------------------------------------------+   +--------------------------------------------+
   |                                    [flip]  |   |                                    [flip]  |
   |  Clarity                                   |   |  +--------------------------------------+  |
   |  ==========O---------------  7             |   |  | 10 Bands                          v |  |
   |                                            |   |  +--------------------------------------+  |
   |  Ambience                                  |   |  Master Gain                              |
   |  =====O--------------------  3             |   |  ==========O-------------- 0 dB           |
   |                                            |   |                                            |
   |  Surround Sound                            |   |  Volume Leveling                          |
   |  ==O-----------------------  1             |   |  =O------------------------ 0.0 dB        |
   |                                            |   |                                            |
   |  Dynamic Boost                             |   |  Filter Q                                 |
   |  ===============O----------  6             |   |  =O------------------------ 1.0x          |
   |                                            |   |                                            |
   |  Bass Boost                                |   |  Balance                                  |
   |  ========O-----------------  4             |   |  ==========O-------------- 0 dB           |
   |                                            |   |  Left                          Right      |
   |                                            |   |  [restore]                                |
   +--------------------------------------------+   +--------------------------------------------+
            face A: FxEffects (default)                   face B: FxEqualizerControl
```

`effects_shown_` starts `true` (`FxAudioControls.cpp:28`), so **face A is the default**. The
`FxEffects` child is `addAndMakeVisible`, the `FxEqualizerControl` child is `addChildComponent`
(invisible) — `FxAudioControls.cpp:32-33`. Both children are given the full panel rect
`(0, 0, 168, 257)` (`FxAudioControls.cpp:80-81`).

In the Pro view the panel sits at x = 40, y = 88 + `visualizer_offset`
(`FxProView.h:325-326`, `FxProView.cpp:441`). The visualizer is 960 × 120 (`FxVisualizer.h:51-52`)
and is always made visible in `FxProView::update()` (`FxProView.cpp:412`), so
`visualizer_offset = 120 + 20 = 140` (`FxProView.cpp:436`) and the panel's effective origin is
**(40, 228)** **(derived)**. The equalizer graph is placed at `audio_controls_.getRight() + 16`
(`FxProView.cpp:442`) → x = 40 + 168 + 16 = **224** **(derived)**.

The panel is **not** present in the Lite view at all — `FxLiteView` only hosts the preset and
playback-device combo boxes (`FxLiteView.cpp:566-581`).

---

## 2. Design tokens the controls depend on

### 2.1 Palette

Two themes, `Dark = 0` and `Light = 1` (`FxTheme.h:28`), indexed into a flat colour table
(`FxTheme.cpp:22-29`). The enum order is `FxTheme.h:29-31`. Colours relevant to this subsystem:

| Token | Dark | Light | Used by |
|---|---|---|---|
| `WindowBackground` | `#181818` | `#f5f5f5` | view background (`FxProView.cpp:455-456`) |
| `DefaultText` | `#b1b1b1` | `#4e4e4e` | all labels, combo text, hotkey text |
| `DefaultFill` | `#000000` | `#ffffff` | text-editor fill, panel tints (always used with an alpha) |
| `HighlightedText` | `#ffffff` | `#000000` | combo text on hover, valid-hotkey text |
| `ControlBackground` | `#0f0f0f` | `#e0e0e0` | the `FxAudioControls` card fill (`FxAudioControls.cpp:88`) |
| `SliderTrack` | `#e33250` | `#0a4d66` | slider track + fill + balance gradient |
| `SliderHighlight` | `#f7546f` | `#53ccff` | keyboard-focus halo (α 0.1) |
| `ImageButton` | `#e63462` | `#23b6eb` | combo arrow tint |
| `ComboBoxBackground` | `#000000` | `#d7d7d7` | combo fill |
| `HintText` | `#7f7f7f` | `#7f7f7f` | preset-name placeholder |
| `ValidTextBorder` | `#009cdd` | `#009cdd` | valid preset name outline |
| `InvalidTextBorder` | `#d51535` | `#d51535` | empty/duplicate preset name outline |
| `PanelBackground` | `#000000` | `#c0c0c0` | Pro-view backing panel at α 0.2 (`FxProView.cpp:458-459`) |

Colour values in the table are RGB only; every consumer applies an explicit alpha via
`.withAlpha(...)`.

### 2.2 Typography

Fonts are bundled TTFs (`FxTheme.cpp:95-97`) and swapped per language (`FxTheme.cpp:382-458`):

| Getter | Face | Height | Citation |
|---|---|---:|---|
| `getNormalFont()` | Gilroy **Semibold** (`font_600_`) | 17.0 px | `FxTheme.cpp:466-469` |
| `getSmallFont()` | Gilroy **Regular** (`font_400_`) | 14.0 px | `FxTheme.cpp:471-474` |
| `getTitleFont()` | Gilroy **Bold** (`font_700_`) | 17.0 px | `FxTheme.cpp:476-479` |
| `getComboBoxFont(box)` | Gilroy Semibold | 14.0 px if `box.getHeight() <= 30`, else 17.0 px | `FxTheme.cpp:120-126` |
| `getPopupMenuFont()` | Gilroy Semibold | 17.0 px | `FxTheme.cpp:367-370` |
| tooltip text | Gilroy Semibold | 14.0 px, wrap at 400 px | `FxTheme.cpp:678-687` |

In this subsystem every caption label is `getNormalFont().withHeight(14)` and every *value* label is
`getNormalFont().withHeight(12.0f)` — i.e. **Gilroy Semibold at 14 px and 12 px**, not the 17 px
default (`FxAudioControls.cpp:104`, `:167`, `:188`, `:305`, `:365`; `FxAudioSlider.cpp:35`;
`FxBalanceSlider.cpp:41`).

Non-Latin languages substitute Noto Sans KR/SC/TC/Thai/JP, Montserrat Alternates (vi) and IBM Plex
Sans Arabic (ar/fa), loaded from files in the CWD (`FxTheme.cpp:390-437`, `:691-704`).

### 2.3 Slider thumb radius

`FxTheme::SLIDER_THUMB_RADIUS = 8` and `ROTARY_SLIDER_THUMB_RADIUS = 5` (`FxTheme.h:44-45`);
`getSliderThumbRadius()` returns 5 for `Rotary` and 8 otherwise (`FxTheme.cpp:320-330`). Every
linear slider in this subsystem therefore uses a **16 px diameter thumb**.

---

## 3. The shared linear-slider engine

All eight sliders in this subsystem (5 effects + Master Gain + Volume Leveling + Filter Q +
Balance) are JUCE `Slider`s in `LinearHorizontal` style with `NoTextBox`, laid out 160 × 18 px.
Everything about their pixel geometry falls out of three functions.

### 3.1 Track rectangle

`FxTheme::getSliderLayout()` takes JUCE's base layout and, for `LinearHorizontal`, shrinks the
width by `SLIDER_THUMB_RADIUS * 4 = 32` (`FxTheme.cpp:344-348`). JUCE's own
`LookAndFeel_V2::getSliderLayout` (framework code, **not in this tree**) starts from the local
bounds and, for a horizontal non-bar slider, reduces horizontally by `thumbIndent =
getSliderThumbRadius() = 8`.

For a 160 × 18 slider **(derived)**:

```
local bounds        (0,  0, 160, 18)
- reduce(8, 0)   →  (8,  0, 144, 18)     JUCE thumbIndent
- width -= 32    →  (8,  0, 112, 18)     FxTheme::getSliderLayout, FxTheme.cpp:346
```

so `sliderBounds = (x=8, y=0, w=112, h=18)`; the **thumb centre travels from x = 8 to x = 120**,
112 px of travel. See §14 for the one-pixel uncertainty in this derivation.

### 3.2 Value → position

JUCE's `Slider::getPositionOfValue(v)` for a horizontal slider returns
`sliderBounds.getX() + proportion(v) * sliderBounds.getWidth()`, i.e. **(derived)**

```
pos(v) = 8 + 112 * (v - min) / (max - min)         // absolute x inside the 160 px component
```

This is an **absolute component coordinate**, not an offset from the track start. Two of the
painters treat it as a width, which produces the artefacts documented in §3.4.

Rust: `fn pos(v: f32, min: f32, max: f32, rect: Rect) -> f32 { rect.left() + rect.width() * (v-min)/(max-min) }`.

### 3.3 Painting (`FxTheme::drawLinearSlider`, `FxTheme.cpp:217-250`)

Called with `x, y, width, height` = the track rect above and `sliderPos = pos(value)`. Order of
operations, with the derived numbers for a 160 × 18 slider:

1. **Unfilled track**: colour `SliderTrack` @ **α 0.2**; if the slider is disabled the colour is
   desaturated with `withSaturation(0.0)` (`FxTheme.cpp:221-225`). Rounded rect at
   `(x, y + (height - 3) / 2, width, 3)` with corner radius **5.6** (`FxTheme.cpp:228`).
   → `(8, 7, 112, 3)` **(derived)**; note `(18 - 3) / 2 = 7` is *integer* division.
2. **Filled track**: same rect but colour `SliderTrack` @ **α 1.0** (desaturated when disabled) and
   **width = `sliderPos`** (`FxTheme.cpp:230-237`).
3. **Thumb**: `SliderThumb` SVG when enabled, `SliderThumbBW` when disabled, drawn within
   `(sliderPos - 8, y + height/2 - 8, 16, 16)` with `RectanglePlacement::centred`
   (`FxTheme.cpp:239-242`) → y = `0 + 9 - 8 = 1`, so the thumb occupies rows 1..17 of the 18 px
   component **(derived)**.
4. **Keyboard-focus halo**: only when `slider.hasKeyboardFocus(true)`. `SliderHighlight` @ α 0.1,
   rounded rect = track rect `.expanded(SLIDER_THUMB_RADIUS/2, SLIDER_THUMB_RADIUS/2)` = expanded
   by 4 px each way, corner radius `height + SLIDER_THUMB_RADIUS = 18 + 8 = 26`
   (`FxTheme.cpp:244-249`) → `(4, -4, 120, 26)` **(derived)**; the 4 px that fall above/below the
   component are clipped away by JUCE's normal child clipping.

The `LinearVertical` branch (`FxTheme.cpp:185-216`, used by the EQ band sliders, a different
subsystem) draws a dashed 1 px line with dash pattern `{5, 2}` and a vertical gradient from
`SliderTrack` α 0.4 to `VerticalSliderLow` α 0.4, plus a focus halo of width `SLIDER_THUMB_RADIUS*4
= 32` with corner radius 20 shown while dragging *or* focused.

### 3.4 Two painting artefacts to reproduce (or consciously fix)

* **Filled-track overshoot.** Because step 2 passes `sliderPos` as the *width* while starting at
  `x`, the fill spans `[8, 8 + pos]` instead of `[8, pos]`. At the minimum value the fill is
  already 8 px wide; at the maximum it reaches x = 128, i.e. **8 px past the end of the unfilled
  track at x = 120** (`FxTheme.cpp:237`) **(derived)**. It stays inside the 160 px component, so it
  never visually clips — it just reads as a slightly over-long bar.
* **Balance gradient end point.** `FxBalanceSlider::paint` builds
  `ColourGradient::horizontal(left, x, right, width)` (`FxBalanceSlider.cpp:89`) — the fourth
  argument is an *x coordinate*, so the gradient runs from x = 8 to **x = 112**, not to the track
  end at x = 120 **(derived)**. The last 8 px of the balance track are painted in the clamped end
  colour.

Recommendation for the port: reproduce these exactly in a "pixel-faithful" mode, but they are
clearly unintentional; a `strict_juce_quirks: bool` flag defaulting to `false` is the pragmatic
choice.

### 3.5 Interaction contract

None of the sliders call `setDoubleClickReturnValue`, `setScrollWheelEnabled`,
`setMouseDragSensitivity`, `setSliderSnapsToMousePosition`, `setVelocityBasedMode`,
`setPopupDisplayEnabled` or `setChangeNotificationOnlyOnRelease` anywhere in `fxsound/Source`
(verified by grep over the whole source tree — zero hits). So all of these behave as JUCE 6.1.6
defaults:

| Gesture | Behaviour | Source |
|---|---|---|
| **Left mouse down on the track** | The slider *snaps to the click position* (`snapsToMousePos` defaults to `true`) and begins a drag | JUCE default |
| **Left drag** | Absolute positional drag (no velocity mode); value snapped to the configured interval | JUCE default |
| **Double-click** | **Nothing.** `doubleClickToValue` defaults to `false` and is never enabled — there is *no* double-click-to-reset in FxSound | absence of `setDoubleClickReturnValue` |
| **Right mouse down** | Reset to the control's default value, **with** notification, on `FxAudioSlider` (`FxAudioSlider.cpp:74-87`) and `FxBalanceSlider` (`FxBalanceSlider.cpp:126-139`). The five effect sliders do **not** override `mouseDown`, so right-click does nothing there | as cited |
| **Mouse wheel** | Enabled (JUCE default). One notch moves the value by roughly `interval` scaled by the wheel delta, clamped to the range | JUCE default |
| **Up arrow / Down arrow** | `setValue(getValue() ± getInterval())`, only when enabled, consuming the key | `FxAudioControls.cpp:249-266`, `FxAudioSlider.cpp:55-72`, `FxBalanceSlider.cpp:107-124` |
| **Left/Right arrow** | Not handled by the overrides; falls through to JUCE's own `Slider::keyPressed`, which also steps by the interval | JUCE default |
| **Focus** | Every slider sets `setWantsKeyboardFocus(true)` (`FxAudioControls.cpp:193`, `FxAudioSlider.cpp:39`, `FxBalanceSlider.cpp:39`) and draws the α-0.1 halo when focused | as cited |
| **Cursor** | `PointingHandCursor` while enabled; effect sliders switch back to `NormalCursor` when disabled (`FxAudioControls.cpp:213-223`) | as cited |

egui note: egui's `Response::double_clicked()` should be left unused here; implement
right-click-reset with `response.secondary_clicked()`, wheel with
`ui.input(|i| i.raw_scroll_delta.y)` gated on `response.hovered()`, and drag with
`response.drag_started()/dragged()` + `interact_pointer_pos()` so that press-to-jump works.

---

## 4. `FxEffects` — the five effect sliders

### 4.1 Enum, order and identity

```cpp
enum EffectType { Fidelity=0, Ambience=1, Surround=2, DynamicBoost=3, Bass=4, NumEffects=5 };
```
`FxAudioControls.h:32`. The DSP mirrors it exactly:
`enum Effect { Fidelity = 0, Ambience = 1, Surround = 2, DynamicBoost = 3, Bass = 4, NumEffects = 5 }`
(`dsp/include/DfxDsp.h:38`). **The order of the five sliders top-to-bottom is the enum order.**

Note the UI label for `Fidelity` is **"Clarity"** — the internal name and the user-visible name
differ. This also shows up in the preset JSON (`FxController.cpp:667-671`: keys `clarity`,
`ambience`, `surround`, `dynamicboost`, `bass`).

### 4.2 Per-slider table

Label texts: `FxAudioControls.cpp:94` (constructor) and `FxAudioControls.cpp:156` (repaint).
Tooltips: `FxAudioControls.cpp:157-161`. `\r\n` in the tooltip is a hard line break.

| # | Enum | Label (`TRANS`) | Range | Step | Default | Displayed as | Tooltip |
|---:|---|---|---|---:|---|---|---|
| 0 | `Fidelity` | `Clarity` | 0 – 10 | 1 | from preset | `%.0f` | `Enhances and elevates high end`<br>`fidelity and presence` |
| 1 | `Ambience` | `Ambience` | 0 – 10 | 1 | from preset | `%.0f` | `Thickens and smooths audio`<br>`with controlled reverberation` |
| 2 | `Surround` | `Surround Sound` | 0 – 10 | 1 | from preset | `%.0f` | `Widens the left-right balance`<br>`for expansive, wide sound` |
| 3 | `DynamicBoost` | `Dynamic Boost` | 0 – 10 | 1 | from preset | `%.0f` | `Increases overall volume and balance`<br>`with responsive processing` |
| 4 | `Bass` | `Bass Boost` | 0 – 10 | 1 | from preset | `%.0f` | `Boosts low end for full,`<br>`impactful response` |

`setRange(0, 10, 1.0)` — `FxAudioControls.cpp:113`. There is **no per-effect default constant**:
the value always comes from the currently loaded preset. `FxEffects::update()` reads
`FxController::getEffectValue()` (a 0.0–1.0 float), rejects anything outside `[0, 1]`, and
multiplies by 10 for the UI (`FxAudioControls.cpp:119-131`).

Tooltips are suppressed entirely when the user has ticked *Hide help tips for audio controls*:
`setTooltip("")` (`FxAudioControls.cpp:169-176`, driven by
`FxController::isHelpTooltipsHidden()`). Tooltip window delay is JUCE's default 700 ms
(`FxProView.cpp:372` constructs `tool_tip_(this)`).

### 4.3 Value scaling across the UI/DSP boundary

This is asymmetric and easy to get wrong:

* `FxController::getEffectValue(e)` returns the DSP's stored value, which is **0.0 … 1.0**
  (`FxController.cpp:1751-1754` → `DfxDspPrivate::getEffectValue`, `dsp/DfxDspPrivate.cpp:231-252`).
* `FxController::setEffectValue(e, v)` expects **0 … 10**, rejects anything outside that
  (`FxController.cpp:1756-1761`), and the DSP divides by 10 before storing
  (`dsp/DfxDspPrivate.cpp:264`, `:270`, `:276`, `:282`, `:288`).
* The DSP additionally sets a per-effect *enable button* to 1 when the value is non-zero and 0 when
  it is zero (`dsp/DfxDspPrivate.cpp:290-300`) — **value 0 means the effect is bypassed**, not
  "applied at zero strength".
* After loading a preset, `FxController::setPreset` re-applies every effect as
  `setEffectValue(e, getEffectValue(e) * 10)` to force the enable-button state to be recomputed
  (`FxController.cpp:1084-1089`).

Rust model: store effect strength as `u8` 0..=10 in the UI layer; convert to `f32 / 10.0` at the
DSP boundary; keep an explicit `enabled = value != 0` per effect if the Rust DSP mirrors the
original's bypass switch.

### 4.4 Side effects of moving an effect slider

`FxEffects::FxEffectSlider::valueChanged()` (`FxAudioControls.cpp:232-247`):

1. Guard: only act if `getValue() != FxController::getEffectValue(effect_) * 10.0`. This is a raw
   float comparison of `float * double`, so in practice it is almost always true after a drag.
2. `FxController::setEffectValue(effect_, value)` → DSP write **and**
   `FxModel::setPresetModified(selectedPreset, true)` + `preset_dirty_ = true`
   (`FxController.cpp:1764-1770`). **Every effect slider move marks the current preset modified**,
   which appends ` *` to the preset combo text (§8.4) and enables the Save/Undo menu items (§8.5).
3. Re-format the value label and re-position it.

`FxEffects::update()` uses `setValue(value, dontSendNotification)`
(`FxAudioControls.cpp:196-206`), which in JUCE does **not** invoke `valueChanged()` — so
programmatic refreshes never mark the preset dirty. The label text and position are updated by hand
in the same function. Reproduce that distinction: a model-driven refresh must not raise the
"modified" flag.

### 4.5 The floating value label

Each effect slider owns a child `Label` (`value_label_`) that is **not** a JUCE text box — it is a
free label that tracks the thumb.

| Property | Value | Citation |
|---|---|---|
| Font | `getNormalFont().withHeight(12.0f)` (Gilroy Semibold 12 px) | `FxAudioControls.cpp:188` |
| Justification | `centredLeft` | `FxAudioControls.cpp:189` |
| Mouse | `setInterceptsMouseClicks(false, false)` — clicks pass through to the slider | `FxAudioControls.cpp:190` |
| Size | `SLIDER_THUMB_RADIUS * 3` = **24** wide × `LABEL_HEIGHT` = **12** high | `FxAudioControls.cpp:229`, `FxAudioControls.h:53` |
| y | `(getHeight() - 12) / 2` = **3** for an 18 px slider **(derived)** | `FxAudioControls.cpp:229` |
| x | `pos(value) + SLIDER_THUMB_RADIUS + 1` = `pos + 9` → **17 … 129** **(derived)** | `FxAudioControls.cpp:204`, `:244` |
| Text | `String::formatted("%.0f", value)` — bare integer, no unit | `FxAudioControls.cpp:200`, `:240` |
| Visibility | `show && isEnabled()` | `FxAudioControls.cpp:208-211` |

JUCE `Label`s carry a default border of `BorderSize<int>(1, 5, 1, 5)` (top, left, bottom, right),
so the glyphs actually start **5 px inside** the label rect: effective text origin x = `pos + 14`
**(derived)**. The caption labels explicitly zero that left border (§4.6); the value labels do not.

Since FxSound 2.0 the values are **always shown** — `FxProView::update()` calls
`audio_controls_.showValues(true)` unconditionally, with the comment
*"Values always visible since version 2.0 (otherwise they do not appear on touch screens)"*
(`FxProView.cpp:414-416`); `FxAudioControls::update()` does the same (`FxAudioControls.cpp:56`).
The `showValues(bool)` plumbing (`FxAudioControls.cpp:61-64`, `:133-139`) survives but is never
called with `false`. **Port decision: draw the value label unconditionally; keep the flag only if a
"compact" mode is wanted later.**

### 4.6 `FxEffects` geometry

Constants: `LABEL_HEIGHT = 14`, `SLIDER_WIDTH = 160`, `SLIDER_HEIGHT = 18`, `X_MARGIN = 8`,
`Y_MARGIN = 21` (`FxAudioControls.h:64-68`). Layout loop `FxAudioControls.cpp:141-152`:

```
label[i]  = (X_MARGIN + SLIDER_THUMB_RADIUS, y, SLIDER_WIDTH, LABEL_HEIGHT)   = (16, y, 160, 14)
slider[i] = (X_MARGIN,                       label.bottom + 1, 160, 18)       = ( 8, y+15, 160, 18)
y_next    = slider.bottom + 10                                                = y + 43
```

Resolved rows **(derived)**:

| i | Effect | caption rect | slider rect |
|---:|---|---|---|
| 0 | Clarity | (16, 21, 160, 14) | (8, 36, 160, 18) |
| 1 | Ambience | (16, 64, 160, 14) | (8, 79, 160, 18) |
| 2 | Surround Sound | (16, 107, 160, 14) | (8, 122, 160, 18) |
| 3 | Dynamic Boost | (16, 150, 160, 14) | (8, 165, 160, 18) |
| 4 | Bass Boost | (16, 193, 160, 14) | (8, 208, 160, 18) |

Last slider bottom = 226; panel height 257 leaves 31 px of empty space at the bottom **(derived)**.
Note that `SLIDER_WIDTH = 160` starting at x = 8 in a 168 px panel means the slider component runs
flush to the right edge; the *caption* label also claims 160 px starting at x = 16 and therefore
overhangs the panel by 8 px — harmless because the captions are short and left-justified.

Caption labels: font `getNormalFont().withHeight(14)`, `Justification::topLeft`, and the default
border's left inset is zeroed so text is flush with the label's left edge
(`FxAudioControls.cpp:104-108`). The caption x of 16 = `X_MARGIN + SLIDER_THUMB_RADIUS` aligns the
text with the **centre of the thumb at value 0**, not with the slider's left edge **(derived)**.

`FxEffects::paint()` does no drawing at all — it re-applies fonts, re-applies the translated label
texts and re-applies/clears tooltips on every repaint (`FxAudioControls.cpp:154-178`). That is how
a language switch propagates. In Rust this becomes "read the strings from the i18n catalogue each
frame", which is free in an immediate-mode GUI.

---

## 5. `FxEqualizerControl` — bands, levels and balance (face B)

### 5.1 Geometry constants

`FxAudioControls.h:93-106`: `X_MARGIN = 8`, `Y_MARGIN = 28`, `ROW_GAP = 8`, `LABEL_WIDTH = 52`
(unused in `resized`), `CONTROL_GAP = 4` (unused), `CONTROL_WIDTH = 100` (unused),
`COMBOBOX_HEIGHT = 20`, `SLIDER_WIDTH = 160`, `SLIDER_HEIGHT = 18`, `LABEL_HEIGHT = 14`,
`BUTTON_WIDTH = 18`, `BUTTON_HEIGHT = 18`, and `equalizer_bands_ = { 5, 10, 15, 20, 31 }`.

Layout (`FxAudioControls.cpp:430-460`), resolved for the 168 px panel where
`width = 168 - 2*8 = 152` **(derived)**:

| Element | Rect | Citation |
|---|---|---|
| EQ-bands combo | (8, 28, 152, 20) | `:436` |
| "Master Gain" caption | (16, 56, 160, 14) | `:439` |
| Master Gain slider | (8, 71, 160, 18) | `:440` |
| "Volume Leveling" caption | (16, 97, 160, 14) | `:443` |
| Volume Leveling slider | (8, 112, 160, 18) | `:444` |
| "Filter Q" caption | (16, 138, 160, 14) | `:447` |
| Filter Q slider | (8, 153, 160, 18) | `:448` |
| "Balance" caption | (16, 179, 160, 14) | `:451` |
| Balance slider | (8, 194, 160, 18) | `:452` |
| "Left" label | (8, 212, 80, 14) | `:455` |
| "Right" label | (88, 212, 48, 14) | `:456` |
| Restore-defaults button | (8, 234, 18, 18) | `:459` |

`SLIDER_WIDTH / 2 = 80`; the Right label's width is `80 - SLIDER_THUMB_RADIUS * 4 = 80 - 32 = 48`
**(derived)** so its right edge lands at x = 136, aligned with the right end of the *track*
(x = 120) plus the thumb half-width... in practice it is pulled 24 px in from the panel edge.
`Right` is `centredRight`-justified with the default right border zeroed (`FxAudioControls.cpp:372-377`),
so the text is flush with x = 136. `Left` is `centredLeft` with the left and top borders zeroed
(`FxAudioControls.cpp:365-370`), flush with x = 8.

Vertical rhythm: every row is `previous slider bottom + ROW_GAP(8)` → caption(14) → `+1` →
slider(18).

### 5.2 The four level sliders

Constructed in the member-initialiser list with their format string and *default* value
(`FxAudioControls.cpp:268-273`):

```cpp
master_gain_slider_   ("%0.0f dB", 0.0f),
volume_leveling_slider_("%.1f dB", 0.0f),
filter_q_slider_      ("%.1fx",   1.0f),
balance_slider_       (0.0f),
```

| Control | Caption (`TRANS`) | Range | Step | Default | Label format | Right-click resets to | Controller setter rounds to |
|---|---|---|---:|---:|---|---:|---|
| Master Gain | `Master Gain` | −20 … +20 | 2 | 0 | `%0.0f dB` | 0.0 | nearest **integer** (`FxController.cpp:1815`) |
| Volume Leveling | `Volume Leveling` | 0 … 4 | 0.5 | 0 | `%.1f dB` | 0.0 | nearest **0.5** (`FxController.cpp:1791`) |
| Filter Q | `Filter Q` | 1 … 3 | 0.5 | 1 | `%.1fx` | 1.0 | nearest **0.5** (`FxController.cpp:1827`) |
| Balance | `Balance` | −20 … +20 | 2 | 0 | `%0.0f dB` of `fabs(v)` | 0.0 | nearest **integer** (`FxController.cpp:1803`) |

Ranges: `FxAudioControls.cpp:312` (master gain), `:330` (volume leveling), `:348` (filter Q),
`FxBalanceSlider.cpp:36` (balance). Captions: `FxAudioControls.cpp:474`, `:477`, `:480`, `:483`.
Defaults also exist as controller constants: `DEFAULT_MASTER_GAIN = 0.0f`,
`DEFAULT_VOLUME_LEVELING = 0.0f`, `DEFAULT_BALANCE = 0.0f`, `DEFAULT_FILTER_Q = 1.0f`,
`DEFAULT_NUM_EQ_BANDS = 10`, `DEFAULT_NORMALIZATION = 0.0f` (`FxController.h:46-51`).
`MIN_GAIN = -12.0f` / `MAX_GAIN = 12.0f` (`FxController.h:52-53`) apply to **EQ band gains**, not
to these controls.

Quirk worth noting: the Master Gain slider's step is **2** while the controller rounds to the
nearest integer, so only even dB values are ever reachable from the slider; the odd values exist
only if a preset or the settings file supplies them.

Each slider's `onValueChange` writes through only when the value actually differs from the
controller's current value (`FxAudioControls.cpp:315-321`, `:333-339`, `:351-357`). The balance
slider does the same inside its own `valueChanged()` (`FxBalanceSlider.cpp:47-56`). These four
setters persist to the settings store (`settings_.setDouble(...)`) and — unlike the effect
sliders — **do not** set the preset-modified flag.

`FxEqualizerControl::update()` refreshes all four with `dontSendNotification`
(`FxAudioControls.cpp:410-420`) and is re-run whenever the face becomes visible
(`FxAudioControls.cpp:546-552`).

### 5.3 `FxAudioSlider` value label

| Property | Value | Citation |
|---|---|---|
| Font | `getNormalFont().withHeight(12.0f)` | `FxAudioSlider.cpp:35` |
| Justification | `centredLeft` | `FxAudioSlider.cpp:36` |
| Size | `LABEL_WIDTH = 40` × `LABEL_HEIGHT = 14` | `FxAudioSlider.h:34-35`, `.cpp:100` |
| y | `(18 - 14) / 2 = 2` **(derived)** | `FxAudioSlider.cpp:100` |
| x | `pos(value) + SLIDER_THUMB_RADIUS/2 + 1` = `pos + 5` → **13 … 125** **(derived)** | `FxAudioSlider.cpp:99` |
| Text | `String::formatted(label_format_, value)` | `FxAudioSlider.cpp:94` |
| Updated on | `resized()` and `valueChanged()` | `FxAudioSlider.cpp:42-53` |

Note the label is 40 px wide and can start at x = 125, so its right edge reaches 165 — 5 px past
the 160 px slider component. With the default 5 px left text inset the glyphs start at x = 130 and
`"20 dB"` at 12 px Gilroy Semibold is ≈ 30 px, so the text just fits; a longer localisation (or a
wider glyph set) will be clipped by the parent. **In the Rust port, clamp the label rect to the
slider rect and right-align the text when it would overflow.**

Unlike the effect sliders this label is a plain visible child (`addAndMakeVisible`,
`FxAudioSlider.cpp:37`) and does **not** disable mouse interception, so a click that lands on the
label does not reach the slider. That is a latent bug; in egui just paint the text, do not create a
separate interactive region.

`FxAudioSlider`'s constructor calls `setValue(default_value)` *before* the owner calls `setRange`
(`FxAudioSlider.cpp:29` vs `FxAudioControls.cpp:312-313`), so it briefly lives under JUCE's default
0..10 range. Harmless for the three values used (0, 0, 1), but do not replicate the ordering.

### 5.4 `FxBalanceSlider` — the custom two-sided track

`FxBalanceSlider` overrides `paint()` entirely (`FxBalanceSlider.cpp:65-105`) instead of using
`FxTheme::drawLinearSlider`:

1. Get the layout from the theme (`FxBalanceSlider.cpp:68`) → `(8, 0, 112, 18)` **(derived)**.
2. `scaled_value = (value + 20) / 40` — the 0..1 normalised position (`FxBalanceSlider.cpp:79`).
3. `left_colour  = SliderTrack.withAlpha(1 - scaled_value)`,
   `right_colour = SliderTrack.withAlpha(scaled_value)` (`FxBalanceSlider.cpp:81-82`). So at hard
   left (−20 dB) the left end is fully opaque and the right end transparent; at centre both are
   α 0.5; at hard right it is mirrored. Disabled → both desaturated (`:83-87`).
4. Horizontal gradient from x = 8 to x = 112 (see §3.4), filled into
   `(8, 7, 112, 3)` corner radius 5.6 (`FxBalanceSlider.cpp:89-92`).
5. Thumb: identical rect maths to the theme — `(pos - 8, 1, 16, 16)` **(derived)**, colour or
   greyscale SVG by enablement (`FxBalanceSlider.cpp:94-97`).
6. Focus halo: `SliderHighlight` α 0.1, track rect expanded by 4, corner radius `height + 8 = 26`
   (`FxBalanceSlider.cpp:99-104`).

There is **no filled/unfilled split** on the balance track — it is one gradient bar whose ends fade.

Value text is `String::formatted("%0.0f dB", std::fabs(value))` — **the absolute value**, so −14 dB
and +14 dB both read `"14 dB"`; the direction is conveyed only by the thumb position and the
Left/Right captions (`FxBalanceSlider.cpp:146`).

Performance bug to *not* copy: `updateValueLabel()` re-parses both thumb SVGs from memory on every
single value change (`FxBalanceSlider.cpp:154-155`). In Rust, rasterise or tessellate the thumb
once and cache it.

`X_MARGIN = 15` is declared in `FxBalanceSlider.h:34` and never used.

### 5.5 EQ-bands combo box

* Items: `{5, 10, 15, 20, 31}` (`FxAudioControls.h:106`), added as `String(bands) + TRANS(" Bands")`
  with **item id == band count** (`FxAudioControls.cpp:298-302`). Display strings: `"5 Bands"`,
  `"10 Bands"`, `"15 Bands"`, `"20 Bands"`, `"31 Bands"`.
* Selection sync: `selectEqualizerBands()` matches the controller's current band count against the
  same five values and falls back to `DEFAULT_NUM_EQ_BANDS = 10` for anything else
  (`FxAudioControls.cpp:509-527`).
* `onChange` maps the selected id back to a band count (defaulting to 10) and calls
  `FxController::setNumEqBands` (`FxAudioControls.cpp:282-296`), which writes the DSP and persists
  `num_bands` in settings (`FxController.cpp:1778-1782`).
* `updateEqualizerBandsText()` re-applies translated item texts on every repaint and re-selects the
  previously selected id if the re-texting cleared the selection (`FxAudioControls.cpp:494-507`).
* Geometry: 152 × 20 → `getComboBoxFont` returns **14 px** (height ≤ 30), corner radius
  `height / 5 = 4`, arrow margin 32 (width 152 > 150) → arrow box `(120, 0, 12, 20)`, text label
  `(5, 1, 110, 18)` **(derived from `FxTheme.cpp:122`, `:138`, `:155-157`, `:161`, `:132`)**.

### 5.6 Restore-defaults button

* 18 × 18 `DrawableButton` in `ImageFitted` style at (8, 234) (`FxAudioControls.cpp:273`, `:459`).
* Normal image `RestoreDefaultsButton`, hover image `RestoreDefaultsButtonHover`
  (`FxAudioControls.cpp:424-427`). Dark theme: `restore_defaults_white.svg` /
  `restore_defaults.svg`; Light theme: `restore_defaults_black.svg` / `restore_defaults_blue.svg`
  (`FxTheme.cpp:35`, `:42`).
* The artwork is a 16 × 16 viewBox counter-clockwise arrow: `M4,7 L1,4 L4,1` arrowhead plus
  `M1,4 L9,4 A5,5 0 1 1 9,14 L2,14` body, 1 px stroke, no fill
  (`fxsound/Images/restore_defaults_white.svg:2,7-8`).
* Tooltip `TRANS("Restore Defaults")` (`FxAudioControls.cpp:491`); cursor PointingHand; takes
  keyboard focus (`FxAudioControls.cpp:379-380`).
* Click → `restoreDefaults()` (`FxAudioControls.cpp:529-544`): sets band count 10, volume leveling
  0, balance 0, filter Q 1, master gain 0 through the controller, then pushes the values back into
  the four sliders and the combo. Note the sliders are updated with the **notifying** `setValue`,
  so their `onValueChange` handlers also fire (harmlessly, since the controller already holds those
  values).
* It resets **only face B**. The five effect sliders are untouched.

### 5.7 Flip button

* 18 × 18 `DrawableButton` (`ImageFitted`) at `(getWidth() - 18 - 5, 5)` = **(145, 5)**
  **(derived)** (`FxAudioControls.h:149-150`, `.cpp:39`, `:83`).
* Images `FlipButton` / `FlipButtonHover`: dark `flip_white.svg` / `flip.svg`, light
  `flip_black.svg` / `flip_blue.svg` (`FxTheme.cpp:35`, `:42`).
* Artwork: 16 × 16 viewBox, two opposed arrows, 1 px stroke —
  `M1,4 L14,4 M11,1 L14,4 L11,7` and `M14,11 L1,11 M4,8 L1,11 L4,14`
  (`fxsound/Images/flip_white.svg:2,7-8`).
* Click toggles `effects_shown_` and the two children's visibility (`FxAudioControls.cpp:41-46`).
  It does not animate. Cursor PointingHand, focusable (`FxAudioControls.cpp:38`, `:40`).
* There is no tooltip on the flip button.

### 5.8 Panel background

`FxAudioControls::paint` fills the whole local bounds with `ControlBackground` at α 1.0, rounded
rect radius **8.0** (`FxAudioControls.cpp:86-90`). Dark `#0f0f0f`, light `#e0e0e0`.

---

## 6. Enable / disable propagation

`FxProView::paint` re-evaluates `FxModel::getPowerState()` every repaint and pushes it onto the
preset combo, the audio-controls panel, the equalizer and the visualizer
(`FxProView.cpp:461-466`). JUCE propagates `setEnabled` down the child tree, so **power off greys
out every control in this subsystem**. `FxLiteView::paint` does the same for the preset combo only
(`FxLiteView.cpp:593-594`).

Disabled rendering rules:

| Element | Disabled appearance | Citation |
|---|---|---|
| Slider track (both layers) | `withSaturation(0.0)` → grey at the same alphas | `FxTheme.cpp:222-225`, `:231-234` |
| Slider thumb | `SliderThumbBW` SVG (grey gradient `#818181`→`#9F9F9F`, stroke `#9D9D9D`→`#7B7B7B`, `#0F0F0F` centre dot r = 3) | `FxTheme.cpp:241-242`, `fxsound/Images/Slider_Thumb_bw.svg:6-13,22` |
| Effect value label | hidden entirely (`show && isEnabled()`) | `FxAudioControls.cpp:210` |
| Effect slider cursor | `NormalCursor` | `FxAudioControls.cpp:220` |
| Combo arrow | `DropDownArrow` (grey `#B0B0B0` chevron) instead of `DropDownArrowHover`, arrow colour α 0.2 | `FxTheme.cpp:159-163` |
| Power button | image drawn at **α 0.5** | `FxPowerButton.cpp:41`, `:47` |
| Hyperlink | text colour `withMultipliedAlpha(0.4f)` | `FxHyperlink.cpp:35` |
| Keyboard | every arrow-key handler is gated on `isEnabled()` | `FxAudioControls.cpp:251`, `FxAudioSlider.cpp:57`, `FxBalanceSlider.cpp:109` |

egui equivalent: `ui.add_enabled_ui(power_on, |ui| ...)` plus explicit desaturated colour
substitution — egui's default "greyed out" tint is a multiply toward the background, which is not
the same as `withSaturation(0.0)`. Implement saturation removal in HSL space to match.

---

## 7. `FxPowerButton`

### 7.1 States and artwork

Two states only, held in `power_state_` (`FxPowerButton.h:46`), default `false`
(`FxPowerButton.cpp:24`).

| State | Image token | Dark asset / fill | Light asset / fill |
|---|---|---|---|
| On | `PowerOnButton` | `power_on.svg`, fill `#E63462` | `power_on_blue.svg`, fill `#23b6eb` |
| Off | `PowerOffButton` | `power_off.svg`, fill `#FFFFFF` | `power_off_black.svg`, fill `#000000` |

Assets: `FxTheme.cpp:33`, `:40`; fills read from
`fxsound/Images/power_on.svg:8`, `power_off.svg:8`, `power_on_blue.svg:8`,
`power_off_black.svg:8`.

Both SVGs are the same 30 × 31 viewBox glyph: a ~300° open ring (path starting
`M19.7231076,2.4807673 …`) plus a vertical bar from y ≈ 0 to y ≈ 14 centred at x ≈ 14.09
(`fxsound/Images/power_on.svg:12-13`). **On and Off differ only in colour, not in shape** — there
is no crossed-out or dimmed variant.

### 7.2 Drawing

`FxPowerButton::paint` (`FxPowerButton.cpp:30-49`):

1. `image_area = (0, 0, image_width_, image_width_)` — a *square* from the single
   `image_width_` field.
2. `RectanglePlacement(xMid | yMid | doNotResize).appliedTo(image_area, getLocalBounds())` centres
   that square in the button without scaling it.
3. `drawWithin(..., stretchToFit | centred, isEnabled() ? 1.0f : 0.5f)` — **`stretchToFit`, so the
   30 × 31 artwork is squashed into a 24 × 24 square** (aspect ratio is *not* preserved)
   **(derived)**.

`image_width_` is set to 24 by the owner (`FxMainWindow.cpp:193`, with
`FxMainWindow::BUTTON_WIDTH = 24` at `FxMainWindow.h:56`); the button itself is also sized 24 × 24
(`FxMainWindow.cpp:192`). `image_width_` is **never initialised in the constructor** — it is
uninitialised memory until `setImageWidth` is called (`FxPowerButton.h:47`, `.cpp:85-88`). Do not
replicate.

`lookAndFeelChanged()` re-creates both drawables from the (now-current) theme and repaints
(`FxPowerButton.cpp:62-67`) — that is how the button follows a dark/light switch.

Because the class derives from `DrawableButton` but overrides `paint` (not `paintButton`), the
`DrawableButton` hover/down image machinery is bypassed entirely: **there is no hover or pressed
artwork for the power button**.

### 7.3 Behaviour

| Aspect | Value | Citation |
|---|---|---|
| Cursor | `PointingHandCursor` | `FxMainWindow.cpp:191` |
| Focus | `setWantsKeyboardFocus(true)` | `FxMainWindow.cpp:195` |
| Keyboard | **Space** triggers a click when enabled; nothing else is consumed | `FxPowerButton.cpp:51-60` |
| Accessible name | `setHelpText(TRANS("Power Button"))` | `FxMainWindow.cpp:194` |
| Click | `power_state = !FxModel::getPowerState(); FxController::setPowerState(power_state); power_button_.setPowerState(model state); repaint()` | `FxMainWindow.cpp:559-567` |
| Model sync | any `FxModel` event re-reads the power state onto the button | `FxMainWindow.cpp:603-606` |
| Disabled when | `!dfx_enabled_` or `SysInfo::isRemoteSession()` — the controller forces power off and disables the button | `FxController.cpp` `setPowerState` (`enablePowerButton(false)`) |
| Disabled tooltip | `TRANS("Audio enhancements are not available over Remote Desktop")`; cleared when re-enabled | `FxMainWindow.cpp:407-418` |

`FxController::setPowerState` → `powerOn(bool)`: on = `dfx_dsp_.powerOn(true)` + start a **100 ms**
timer; off = `dfx_dsp_.powerOn(false)`, stop the timer, and
`audio_passthru_->restoreDefaultPlaybackDevice()`. It also persists `settings_.setBool("power", …)`
and updates the tray icon and the window icon.

### 7.4 Placement in the title bar

The title bar is `FxTheme::TITLE_BAR_HEIGHT - 1 = 56` px tall (`FxTheme.h:43`, `FxWindow.cpp:29`)
and spans `x = WINDOW_CORNER_RADIUS(21) + shadow` to `width - 42 - 2*shadow`
(`FxWindow.cpp:148`); the main window disables its shadow so `shadow = 0`
(`FxMainWindow.cpp:183`, `FxWindow.cpp:100-111`).

Right-aligned toolbar buttons are stacked right-to-left starting at
`close_button_.getWidth() + 20 = 15 + 20 = 35` and advancing by `button width + 20`
(`FxWindow.cpp:286-308`, `CLOSE_BUTTON_WIDTH = 15` at `FxWindow.h:45`). Registration order is
menu (left-aligned), minimize, resize, **power**, donate (`FxMainWindow.cpp:233-237`), with sizes
26 × 30 (minimize/donate, `BUTTON_WIDTH + 2` × `BUTTON_WIDTH + 6`), 26 × 26 (resize in Pro) or
24 × 24 (resize in Lite), and 24 × 24 (power). So in Pro view, measuring right edges from the title
bar's right edge **(derived)**: minimize −35, resize −81, **power −127**, donate −171.

---

## 8. Combo boxes and the preset list

### 8.1 `FxComboBox`

A thin `ComboBox` subclass (`FxComboBox.h:26-44`):

* Cursor `PointingHandCursor`; text colour initialised to the scheme's `defaultText`
  (`FxComboBox.cpp:26-29`).
* `highlightText(bool)` — switches the text colour between `highlightedText` and `defaultText`, but
  **returns early with `defaultText` if the currently selected item is disabled**
  (`FxComboBox.cpp:36-54`). Driven from `FxView::mouseEnter/mouseExit` on hover
  (`FxView.cpp:258-277`).
* `setError(bool)` — repaints the outline in `SliderTrack` α 1.0 on error, `DefaultFill` α 1.0
  otherwise (`FxComboBox.cpp:61-73`). Used for an unavailable playback device.
* `onShowPopup` hook fired just before the popup opens (`FxComboBox.cpp:75-83`); the playback-device
  combo uses it to rescan devices (`FxView.cpp:102-104`).

### 8.2 Combo painting (`FxTheme::drawComboBox`, `FxTheme.cpp:135-164`)

```
cornerSize = height / 5                                   // float division of an int height
fillRoundedRectangle(0, 0, width, height, cornerSize)      // ComboBox::backgroundColourId
drawRoundedRectangle(inset 0.5, cornerSize, 1.0f)          // focusedOutline if focused else outline
margin = (width <= 150) ? 24 : 32
arrow SVG drawn within (width - margin, 0, 12, height), RectanglePlacement::centred
```

Colours from `FxTheme::init()` (`FxTheme.cpp:73-77`): background `ComboBoxBackground` α 1.0,
outline `ComboBoxBackground` α 1.0 (invisible by default), focused outline `SliderHighlight`
α **0.2**, arrow `ImageButton` α 1.0 (α 0.2 when disabled), text `DefaultText` α 1.0.

Arrow artwork: enabled uses `DropDownArrowHover` (dark `dropdown_arrow_hover.svg`, an 11 × 7 filled
chevron `#E63462`; light `dropdown_arrow_hover_blue.svg`), disabled uses `DropDownArrow`
(`dropdown_arrow_bw.svg`, an 11 × 7 stroked `#B0B0B0` polyline `0,0 → 4.8,4.8 → 9.6,0`)
(`FxTheme.cpp:36`, `:43`, `:99-102`, `:159-163`;
`fxsound/Images/dropdown_arrow_hover.svg:2,8`, `dropdown_arrow_bw.svg:2,8`).

Text position: JUCE's `LookAndFeel_V4::positionComboBoxText` then overridden to
`x = 5, right = box.width - 37` (`FxTheme.cpp:128-133`), with
`label.setMinimumHorizontalScale(1.0)` so the text is **never squeezed** — long preset names are
truncated with an ellipsis instead.

Placeholder text when nothing is selected is drawn at α 0.5 from x = 10
(`FxTheme.cpp:166-179`); both lists set it to the empty string (`FxView.cpp:86`, `:95`).

Resolved combo geometry **(derived)**:

| Combo | Rect | corner | font | arrow box | text box |
|---|---|---:|---:|---|---|
| Preset (Pro) | (40, 32, 470, 40) | 8 | 17 px | (438, 0, 12, 40) | (5, 1, 428, 38) |
| Playback device (Pro) | (530, 32, 470, 40) | 8 | 17 px | (438, 0, 12, 40) | (5, 1, 428, 38) |
| Preset (Lite) | (40, 42, 225, 50) | 10 | 17 px | (193, 0, 12, 50) | (5, 1, 183, 48) |
| Playback device (Lite) | (285, 42, 225, 50) | 10 | 17 px | (193, 0, 12, 50) | (5, 1, 183, 48) |
| EQ bands | (8, 28, 152, 20) | 4 | 14 px | (120, 0, 12, 20) | (5, 1, 110, 18) |

Pro coordinates: `FxProView.h:322-328`, `FxProView.cpp:426-430`. Lite coordinates:
`FxLiteView.h:528-530` with `FxView::LIST_WIDTH = 225` / `LIST_HEIGHT = 50` (`FxView.h:39-40`,
`FxView.cpp:87`, `:96`, `FxLiteView.cpp:571-580`).

### 8.3 Popup menu styling

`drawPopupMenuItem` forces the highlight background on ticked items and additionally strokes a
rectangle around the ticked row in `PopupMenu::textColourId` α 1.0 (`FxTheme.cpp:353-365`).
Popup background `DefaultFill` α 1.0, highlight background `ImageButton` α 1.0
(`FxTheme.cpp:89-90`). Font 17 px Semibold (`FxTheme.cpp:367-370`). Every popup child gets the
pointing-hand cursor (`FxTheme.cpp:372-380`).

### 8.4 Preset list population, ordering and the "modified" marker

Populated in `FxView::modelChanged(PresetListUpdated)` (`FxView.cpp:205-225`):

```cpp
preset_list_.clear(dontSendNotification);
auto preset_type = FxModel::PresetType::AppPreset;
for (i = 0 .. count-1) {
    preset = model.getPreset(i);
    if (preset_type != preset.type) { preset_list_.addSeparator(); preset_type = preset.type; }
    name = preset.modified ? preset.name + L" *" : preset.name;
    preset_list_.addItem(name, i + 1);
}
preset_list_.setSelectedId(model.getSelectedPreset() + 1, dontSendNotification);
```

Rules to reproduce exactly:

1. **Item id = array index + 1.** Id 0 means "nothing selected" in JUCE, so the off-by-one is
   load-bearing.
2. **Ordering is the model's array order**, which is: all factory presets first, then all user
   presets. `FxController::initPresets` scans `<cwd>/Factsoft/*.fac` as `AppPreset`, then
   `<userAppData>/FxSound/Presets/*.fac` as `UserPreset` (`FxController.cpp:841-875`). Within each
   group the order is whatever `FileSearchPath::findChildFiles` returns — **not explicitly
   sorted**. The display name comes from inside the `.fac` file
   (`dfx_dsp_.getPresetInfo(...).name`), not from the filename.
3. **Exactly one separator** is emitted, at the AppPreset→UserPreset boundary, because
   `preset_type` starts at `AppPreset` and only ever changes once.
4. **Modified marker is the literal suffix `" *"`** (space, asterisk) appended to the name.
5. A preset is marked modified at load time if an auto-saved copy of it exists on disk
   (`FxController.cpp:869-873`).

Live marker updates, `FxView::modelChanged(PresetModified)` (`FxView.cpp:227-255`): the selected
item's text is changed to `name + " *"` or back to `name`, **and** — only if the popup is not
currently open — the combo's displayed text is set directly with
`setText(..., dontSendNotification)`. Empty preset names are skipped.

Selection: `FxView::comboBoxChanged` maps the selected *index* (not id) to
`FxController::setPreset(index)` (`FxView.cpp:135-149`). `FxModel::Event::PresetSelected` writes
`setSelectedId(selected + 1, dontSendNotification)` back (`FxView.cpp:200-203`), and in the Pro view
also triggers a full `update()` of the controls (`FxProView.cpp:474-482`).

Switching presets auto-saves the outgoing one if it was modified, and prefers an auto-saved copy
over the original when loading (`FxController.cpp:1048-1086`). On success it pushes a toast
`TRANS("Preset: ") + name` when the power is on (`FxController.cpp:1098-1101`).

### 8.5 Save / undo affordances

**There are no save or undo buttons next to the combo box.** Everything lives in the hamburger
menu built by `FxMainWindow::showMenu()` (`FxMainWindow.cpp:509-551`). Menu contents, in order,
with their exact enable predicates (`model` = `FxModel`, `power_state` = `model.getPowerState()`):

| Item | Enabled when | Action |
|---|---|---|
| `Settings` | always | modal `FxSettingsDialog`, then refresh outputs |
| — separator — | | |
| `Save New Preset` (submenu) | `model.isPresetModified() && model.getUserPresetCount() < maxUserPresets && power_state` | submenu contains a single custom text-field item (§11) |
| `Overwrite Existing Preset` (+ `" - " + preset.name` when modified and user preset) | `model.isPresetModified() && user_preset && power_state` | `FxController::savePreset()` with an empty name → overwrite in place |
| `Undo Preset Changes` | `model.isPresetModified() && power_state` | `FxController::undoPreset()` |
| `Rename Preset` (submenu) | `!model.isPresetModified() && user_preset && power_state` | submenu with a text field |
| `Delete Preset` | `user_preset && power_state` | `FxController::deletePreset()` |
| — separator — | | |
| `Export Presets` | `!model.isPresetModified() && power_state` | modal export dialog |
| `Import Presets` | `!model.isPresetModified() && power_state` | modal import dialog |
| — separator — | | |
| `Download Bonus Presets` | always | opens `https://www.fxsound.com/presets` |
| — separator — | | |
| `Check for updates` | always | runs `updater.exe /checknow` |
| — separator — | | |
| `Theme` → `Dark` / `Light` | always, ticked by current mode | `FxController::setThemeMode` |
| `Always On Top` | always, ticked by current state | toggles always-on-top |
| — separator — | | |
| `Donate` | always | opens a PayPal URL |

`maxUserPresets` is read from settings and clamped: anything below 10 or above 120 becomes **120**
(`FxController.cpp:194-198`).

`undoPreset()` (`FxController.cpp:1317-1332`): no-op if not modified; otherwise clear the modified
flag, delete the auto-saved copy, and re-run `setPreset(index)` so the original `.fac` is reloaded.

`savePreset("")` (`FxController.cpp:1204-1220`): re-save the DSP state under the existing preset
name into `<userAppData>/FxSound/Presets`, delete the auto-save, clear the modified flag, toast
`"Changes to preset %s are saved."`. `savePreset(name)` (`:1221-1241`) writes a new `.fac`,
re-enumerates presets, selects the new one, toasts `"New preset %s is saved."`, and if the user
preset count has just hit the cap, sleeps 2 s and toasts `"Reached the limit on new presets."`.

The menu button also shows a one-shot help bubble on first hover:
`TRANS("Click here to save new presets, overwrite old ones, or reset your settings.")`
(`FxMainWindow.cpp:590-601`).

**Linux/PipeWire note.** Nothing in the preset combo is Windows-specific except the storage paths
and `SHFileOperation`-based deletes (`FxController.cpp:1252-1258`, `:1285-1291`). Map
`userApplicationDataDirectory` → `$XDG_DATA_HOME/fxsound` (default `~/.local/share/fxsound`) and
the factory `Factsoft/` directory → `/usr/share/fxsound/presets` with a per-user override;
`SHFileOperation(FO_DELETE)` → `std::fs::remove_file` (or the trash via the
`org.freedesktop.portal.Trash` portal if a recoverable delete is wanted).

---

## 9. `FxHotkeyLabel` / `FxHotkeyEditor`

### 9.1 Composition and geometry

`FxHotkeyLabel` is a plain `Component` holding a caption `Label` and a `FxHotkeyEditor`
(`FxHotkeyLabel.h:44-58`).

```
HOTKEY_LABEL_WIDTH = 170                      FxHotkeyLabel.h:50
HOTKEY_EDITOR_WIDTH = 120, HEIGHT = 20        FxHotkeyLabel.h:28-29

+----------------------------------------+ +----------------------------+
| Turn FxSound On/Off                    | |     Ctrl +  Shift + Q      |
+----------------------------------------+ +----------------------------+
 x = bounds.x, w = 170, h = editor height    x = label.right + 1, 120 x 20
```

`resized()` (`FxHotkeyLabel.cpp:34-43`): caption at `(bounds.x, bounds.y, 170, editor.height)`;
editor keeps its own width/height (120 × 20 from the constructor, `FxHotkeyLabel.cpp:73`) and is
moved to `(label.right + 1, label.y)`.

Caption font is `getSmallFont()` = Gilroy **Regular 14 px**, `Justification::topLeft`, re-applied
on every paint so a language switch takes effect (`FxHotkeyLabel.cpp:26-28`, `:47-51`).

In the settings dialog the five labels are stacked at `HOTKEY_LABEL_X = X_MARGIN + 25 = 45`,
height `HOTKEY_LABEL_HEIGHT = 20`, pitch `20 + 10 = 30`, width `paneWidth - 45`, starting 5 px
below the "Disable keyboard shortcuts" toggle (`FxSettingsDialog.h:79`, `:139-141`,
`FxSettingsDialog.cpp:441-446`).

### 9.2 The five commands and their defaults

Command keys (`FxController.h:54-58`) and Win32 hotkey ids (`FxController.h:219-223`):

| # | Settings key | Constant | Win32 id | Caption in settings | Default code | Decoded default |
|---:|---|---|---:|---|---:|---|
| 0 | `cmd_on_off` | `HK_CMD_ON_OFF` | 1001 | `Turn FxSound On/Off` | `393297` | **Ctrl + Shift + Q** |
| 1 | `cmd_open_close` | `HK_CMD_OPEN_CLOSE` | 1002 | `Open/Close FxSound` | `393285` | **Ctrl + Shift + E** |
| 2 | `cmd_next_preset` | `HK_CMD_NEXT_PRESET` | 1003 | `Use Next Preset` | `393281` | **Ctrl + Shift + A** |
| 3 | `cmd_previous_preset` | `HK_CMD_PREVIOUS_PRESET` | 1004 | `Use Previous Preset` | `393306` | **Ctrl + Shift + Z** |
| 4 | `cmd_change_output` | `HK_CMD_NEXT_OUTPUT` | 1005 | `Change Playback Device` | `393303` | **Ctrl + Shift + W** |

Default codes: `fxsound/Source/Utils/Settings/Settings.cpp:34-38`. Captions and their order:
`FxSettingsDialog.cpp:342-345`.

**Encoding:** `code = (mod << 16) | vk`; decoding is `mod = (code >> 16) & 0x7`, `vk = code & 0xff`
(`FxController.cpp:2176-2189`, `setHotkey` at `:2212`). The modifier bits are the Win32
`MOD_ALT = 0x1`, `MOD_CONTROL = 0x2`, `MOD_SHIFT = 0x4`. All five defaults use `mod = 6` =
`MOD_CONTROL | MOD_SHIFT` **(derived: 393297 = 0x60051, 393285 = 0x60045, 393281 = 0x60041,
393306 = 0x6005A, 393303 = 0x60057)**.

`getHotkey` only reports a hotkey as present if `mod` is exactly `Ctrl+Alt` or `Ctrl+Shift` **and**
`vk` is `0x30..0x39` (`0`–`9`) or `'A'..'Z'`; otherwise it zeroes both outputs and returns false
(`FxController.cpp:2181-2190`).

### 9.3 What each hotkey does

`FxController::eventCallback`, `WM_HOTKEY` branch:

* `CMD_ON_OFF` — toggles power unless in a remote session, then toasts
  `FormatString(TRANS("FxSound is %s."), TRANS("on"|"off"))`.
* `CMD_OPEN_CLOSE` — hides the main window if it is on the desktop, otherwise shows it.
* `CMD_NEXT_PRESET` — only when powered on and `presetCount > 1`; index + 1 with wrap to 0.
* `CMD_PREVIOUS_PRESET` — only when powered on and `presetCount > 1`; index − 1 with wrap to
  `count - 1`.
* `CMD_NEXT_OUTPUT` — advances through `active_output_devices_` with wrap.

### 9.4 Capture rules (`FxHotkeyEditor::keyPressed`, `FxHotkeyLabel.cpp:76-173`)

1. **Delete** clears the binding: `mod = vk = 0`, `setHotkey(command, 0, 0)`, refresh text, consume
   the key (`FxHotkeyLabel.cpp:78-85`).
2. Build `mod` from the live modifiers: Ctrl → `MOD_CONTROL`; then `if Alt → |= MOD_ALT` **`else if`
   Shift → `|= MOD_SHIFT`** — note Alt wins and Shift is ignored when both are held
   (`FxHotkeyLabel.cpp:89-102`).
3. Reject (return `false`, key not consumed) unless Ctrl is present **and** at least one of
   Alt/Shift is present (`FxHotkeyLabel.cpp:104-107`).
4. Accept the key code only if it is `'0'..'9'` (`0x30..0x39`) or `'A'..'Z'`
   (`FxHotkeyLabel.cpp:109-113`).
5. Reject the combination if it equals any of the five currently stored hotkeys — checked one by
   one against `HK_CMD_ON_OFF`, `HK_CMD_OPEN_CLOSE`, `HK_CMD_NEXT_PRESET`,
   `HK_CMD_PREVIOUS_PRESET`, `HK_CMD_NEXT_OUTPUT` (`FxHotkeyLabel.cpp:115-152`). Note this
   includes the editor's *own* command, so **re-entering the same shortcut is refused** rather than
   being a no-op.
6. Commit: `FxController::setHotkey(command, mod, vk)`; if that fails, reset the local `mod_`/`vk_`
   to 0 (`FxHotkeyLabel.cpp:154-170`).

`FxController::setHotkey` (`FxController.cpp:2192-2270`) repeats the duplicate check (skipping the
command being edited this time), unregisters the old Win32 hotkey, and registers the new one; on
`RegisterHotKey` failure it stores 0 and returns false.

`FxController::isValidHotkey` (`FxController.cpp:2271-2295`) requires `MOD_CONTROL` and then uses
`ToUnicodeEx` with a synthetic keyboard state to check whether the combination would produce a
printable character (`output[0] >= 0x20`); if it would, the hotkey is **rejected**. This exists to
stop Ctrl+Alt (= AltGr on many layouts) from stealing character input.

### 9.5 Rendering (`FxHotkeyEditor::paint`, `FxHotkeyLabel.cpp:185-222`)

1. `Label::paint(g)` draws the current key text — `Justification::centred`, `getSmallFont()`
   (Gilroy Regular 14 px) (`FxHotkeyLabel.cpp:66-67`).
2. Border thickness = **2** when the editor has keyboard focus, **1** otherwise; forced back to 1
   when disabled (`FxHotkeyLabel.cpp:193-200`, `:214-217`).
3. Text colour: `TextEditor::highlightedTextColourId` (= `HighlightedText`, `#ffffff` dark /
   `#000000` light) when enabled *and* `FxController::isValidHotkey(mod_, vk_)`; otherwise
   `TextEditor::textColourId` (= `DefaultText`) (`FxHotkeyLabel.cpp:202-218`, colour registration
   at `FxTheme.cpp:81-82`).
4. Border: `drawRoundedRectangle(bounds.reduced(t, t), cornerRadius = 5.0f, thickness = t)` in
   `TextEditor::textColourId` (`FxHotkeyLabel.cpp:220-221`). `t` is a `float` narrowed to `int` by
   `Rectangle<int>::reduced`.

`focusGained` / `focusLost` simply repaint (`FxHotkeyLabel.cpp:175-183`).
`setMouseClickGrabsKeyboardFocus(true)` + `setWantsKeyboardFocus(true)` (`FxHotkeyLabel.cpp:62-63`)
make a single click arm the capture.

### 9.6 Key text formatting (`setKeyText`, `FxHotkeyLabel.cpp:224-257`)

```
if (mod_ == 0)                  key_text_ = TRANS("Not configured")
else {
    if (mod_ & MOD_CONTROL)     key_text_  = " " + TRANS("Ctrl")  + " + "
    if (mod_ & MOD_ALT)         key_text_ += " " + TRANS("Alt")   + " + "
    if (mod_ & MOD_SHIFT)       key_text_ += " " + TRANS("Shift") + " + "
    if (vk in '0'..'9' | 'A'..'Z')  key_text_ += (wchar_t)vk_
}
```

So the default on/off binding renders literally as `" Ctrl +  Shift + Q"` — leading space, **two
spaces** between `+` and `Shift`. Reproduce it or clean it up deliberately; do not do so by
accident.

Tooltip on the editor: `TRANS("Press Ctrl + Alt/Shift + 0-9/A-Z to change the hotkey")`
(`FxHotkeyLabel.cpp:70`).

### 9.7 Global enable switch

The settings pane's `Disable keyboard shortcuts` toggle calls
`FxController::enableHotkeys(!toggleState)` and then enables/disables all five
`FxHotkeyLabel`s (`FxSettingsDialog.cpp:386-395`, `FxController.cpp:2810-2822`). If
`SysInfo::canSupportHotkeys()` is false the toggle is forced on and disabled
(`FxSettingsDialog.cpp:375-383`). Registration/unregistration is all-or-nothing over the five ids,
guarded by `hotkeys_registered_` (`FxController.cpp:2820-2884`).

### 9.8 Linux / Wayland substitute — this is the biggest gap in the subsystem

`RegisterHotKey`/`UnregisterHotKey`/`WM_HOTKEY` on a hidden message-only `HWND`
(`FxController.h:180-217`) has **no equivalent available to a Wayland client**. A Wayland client
cannot grab keys it does not have focus for; there is no `XGrabKey`. Concretely:

1. **Primary mechanism: `org.freedesktop.portal.GlobalShortcuts` (XDG desktop portal, interface
   version 1+).** Call `CreateSession`, then `BindShortcuts` with the five shortcut ids
   (`cmd_on_off`, `cmd_open_close`, `cmd_next_preset`, `cmd_previous_preset`, `cmd_change_output`),
   each with a human-readable `description` (reuse the five captions from §9.2) and a
   `preferred_trigger` string in the portal's syntax — e.g. `"CTRL+SHIFT+q"`,
   `"CTRL+SHIFT+e"`, `"CTRL+SHIFT+a"`, `"CTRL+SHIFT+z"`, `"CTRL+SHIFT+w"`. Listen to the
   `Activated` signal and dispatch on the shortcut id exactly as `eventCallback` dispatches on
   `w_param`. Use the `ashpd` crate.
2. **The compositor, not the app, owns the binding UI.** The portal only accepts a *preference*;
   the actual trigger is whatever the compositor assigns, and it is reported back in
   `ShortcutsChanged` / in the `BindShortcuts` reply as `trigger_description`. So
   `FxHotkeyEditor` must become a **read-only display** of `trigger_description` plus a
   "Change in system settings…" affordance that calls the portal's own configuration entry point
   (GNOME/KDE expose one) — the in-place key-capture editor cannot work. Keep the widget's visual
   design (120 × 20 rounded box, radius 5, 1/2 px border, 14 px centred text) and the
   `"Not configured"` empty state; drop steps 1–6 of §9.4.
3. **Fallback when no portal is available** (bare `wlroots` without
   `xdg-desktop-portal-wlr`'s shortcuts support): expose the five commands on a D-Bus interface
   (`org.fxsound.Fxsound1` with methods `TogglePower`, `ToggleWindow`, `NextPreset`,
   `PreviousPreset`, `NextOutput`) and document the `hyprctl`/`swaymsg`/`kwriteconfig` one-liners
   the user must add to their compositor config. A second fallback is MPRIS
   (`org.mpris.MediaPlayer2.fxsound`) for the play-adjacent actions only — but MPRIS has no
   semantics for "next preset", so do not overload it.
4. **X11 sessions**: if the app detects an X11 backend (`winit` reports it), a classic `XGrabKey`
   path can still reproduce the original behaviour exactly, including the modifier encoding. Ship
   it as an optional feature, not the default.
5. **`isValidHotkey`'s `ToUnicodeEx` check** maps to `xkbcommon`: build an `xkb_state`, apply the
   modifier mask, and reject the combination if `xkb_state_key_get_utf8` yields a codepoint
   ≥ 0x20. Only needed on the X11 fallback path.
6. **Settings encoding**: keep the `(mod << 16) | vk` integer for backwards compatibility when
   importing a Windows settings file, but store the Linux binding as a portal shortcut id +
   trigger description string.

---

## 10. `FxHyperlink`

A `HyperlinkButton` that paints itself instead of using the JUCE default
(`FxHyperlink.h:27-39`, `.cpp:27-42`):

* Colour = the **theme's `defaultText`** (not `HyperlinkButton::textColourId`, which the theme sets
  to `HighlightedText` at `FxTheme.cpp:87` and which this override ignores);
  `withMultipliedAlpha(0.4f)` when disabled (`FxHyperlink.cpp:30-35`).
* Font = `getNormalFont()` (Gilroy Semibold **17 px**) with `setUnderline(true)`
  (`FxHyperlink.cpp:37-39`).
* Drawn with `g.drawText(text, getLocalBounds(), getJustificationType(), true)` — single line,
  ellipsis on overflow (`FxHyperlink.cpp:41`).
* `getTextWidth()` measures the underlined font's string width so callers can size the button
  (`FxHyperlink.cpp:44-50`).
* No hover or visited styling at all — the two `bool` parameters of `paintButton` are unnamed and
  unused.

Used by the error notification (`FxNotification.h:55`), the toast message
(`FxMessage.h:62`), the Help settings pane's five links (`FxSettingsDialog.h:178-182`) and the
controller's error dialog (`FxController.cpp:115-116`). It is **not** used inside the effect-control
panel.

Linux note: `URL::launchInDefaultBrowser()` → `xdg-open` via the `open` crate, or better the
`org.freedesktop.portal.OpenURI` portal so it works inside a Flatpak sandbox.

## 11. `FxPresetNameEditor` and `PresetNameInputFilter`

### 11.1 Status

`FxPresetNameEditor` (200 × 30, `FxPresetNameEditor.h:39-40`) is **dead code in the shipping UI**.
`FxMainWindow.cpp` includes the header (`FxMainWindow.cpp:25`) only to get
`PresetNameInputFilter`; the actual in-menu text field is a byte-for-byte duplicate implemented as
`FxPresetMenuItem : PopupMenu::CustomComponent` inside `FxMainWindow.cpp:27-176`. Everything below
therefore describes both.

### 11.2 Input filter

`PresetNameInputFilter` strips these nine characters from any inserted text:
`<` `>` `:` `"` `/` `\` `|` `?` `*` (`FxPresetNameEditor.cpp:6-16` constructor,
filtering loop at `:20-33`). That is the Windows reserved-filename set, because the preset name
becomes the `.fac` filename.

**Linux port:** only `/` and NUL are actually illegal in a POSIX filename, but keeping the Windows
set is the right call for preset-file interchange with Windows FxSound. Additionally reject a
leading `.` and the names `.` and `..`, and normalise to NFC.

Length cap: `setInputRestrictions(64)` — **64 characters** (`FxPresetNameEditor.cpp:52`).

### 11.3 Three-state validation

`enum class Status { Empty = 0, Valid, Invalid }` (`FxPresetNameEditor.h:37`).
`textEditorTextChanged` (`FxPresetNameEditor.cpp:93-123`):

| Condition | Status | Hint label alpha |
|---|---|---:|
| text is empty | `Empty` | 1.0 (placeholder visible) |
| text matches an existing preset name, case-insensitively | `Invalid` | 0.0 |
| otherwise | `Valid` | 0.0 |

On a status transition it calls `sendLookAndFeelChange()`, which routes to `lookAndFeelChanged()` →
`repaint()` (`FxPresetNameEditor.cpp:119-128`).

### 11.4 Painting (`FxPresetNameEditor.cpp:68-91`)

```
outline = (status == Valid) ? ValidTextBorder(#009cdd) : InvalidTextBorder(#d51535)   // both α 1.0
fill    = findColour(TextEditor::backgroundColourId)      // DefaultFill α 1.0, FxTheme.cpp:78
g.fillRect(localBounds)                                   // square corners, not rounded
g.drawRect(localBounds.reduced(0.5, 0.5), 2.0f)           // 2 px outline
preset_editor_.grabKeyboardFocus()                        // ← inside paint()
```

Note `Empty` and `Invalid` share the red outline — the field is red until a usable unique name is
typed. Also note the focus grab happens **in `paint`**, which forces focus back on every repaint;
in an immediate-mode port, request focus once on open instead.

Inner layout: both the hint label and the editor occupy `localBounds.reduced(2, 2)` — i.e. they
overlap exactly, and the hint is hidden by alpha rather than by visibility
(`FxPresetNameEditor.cpp:60-66`).

Hint label: `getNormalFont()` (17 px Semibold), colour `HintText` `#7f7f7f` α 1.0, `centredLeft`,
text `TRANS("Enter your preset name")` (`FxPresetNameEditor.cpp:41-48`). The *rename* variant in
the menu uses `TRANS("Enter new preset name")` (`FxMainWindow.cpp:52-53`).
Editor background is `DefaultFill` at **α 0.0** — fully transparent, so the panel fill shows
through (`FxPresetNameEditor.cpp:51`).

### 11.5 Keys (menu variant only)

* **Escape** → set status `Empty` and `triggerMenuItem()` (closes the menu without saving)
  (`FxMainWindow.cpp:63-66`).
* **Return** → only when status is `Valid`: `FxController::savePreset(name)` or
  `renamePreset(name)` depending on the action, then `triggerMenuItem()`
  (`FxMainWindow.cpp:68-81`).

There is no OK/Cancel button — Return and Escape are the entire interaction.

---

## 12. Tooltips

`FxProView` owns the single `TooltipWindow` for the whole Pro view, parented to itself with JUCE's
default 700 ms delay (`FxProView.h:340`, `.cpp:372`), and forces the text colour to the scheme's
`defaultText` with `setOpaque(false)` (`FxProView.cpp:381-384`).

`FxTheme::getTooltipBounds` (`FxTheme.cpp:515-526`): size = laid-out text size + (20, 12);
position = `screenPos.x + 36` (or `screenPos.x - (w + 18)` past the horizontal centre of the
parent) and `screenPos.y + 12` (or `screenPos.y - (h + 12)` past the vertical centre), then
constrained to the parent area.

`FxTheme::drawTooltip` (`FxTheme.cpp:528-541`): rounded rect fill radius **5.0**, 1 px outline
inset by 0.5, text drawn inside `bounds.reduced(10, 0)`.

Text layout (`FxTheme.cpp:676-689`): word wrap, `centredLeft`, font
`getNormalFont().withHeight(14.0f)`, max width **400** px.

Tooltip strings in this subsystem: the five effect tooltips (§4.2), `Restore Defaults`
(`FxAudioControls.cpp:491`), the hotkey editor hint (`FxHotkeyLabel.cpp:70`), `Donate`
(`FxMainWindow.cpp:212`) and the remote-desktop power message (`FxMainWindow.cpp:413`).

---

## 13. Rust / egui implementation guidance

### 13.1 Suggested decomposition

```
controls/
  slider.rs      // fx_slider(ui, &mut value, Spec) -> Response   — §3 painter + interaction
  effects.rs     // FxEffectsPanel  — 5 sliders, §4
  levels.rs      // FxLevelsPanel   — combo + 4 sliders + restore, §5
  card.rs        // FxAudioControls — 168x257 card, flip button, face switch, §1
  power.rs       // FxPowerButton   — §7
  combo.rs       // FxComboBox      — §8
  hotkeys.rs     // portal-backed shortcut list, §9
  preset_name.rs // validated name field, §11
theme.rs         // palette tables from §2.1, Gilroy faces, token accessors
```

### 13.2 Slider spec type

```rust
pub struct SliderSpec {
    pub min: f32,
    pub max: f32,
    pub step: f32,            // 1.0 | 2.0 | 0.5
    pub default: Option<f32>, // Some(..) enables right-click reset
    pub format: ValueFormat,  // Int | OneDp | OneDpX | IntDb | AbsIntDb
    pub label_dx: f32,        // 9.0 for effect sliders, 5.0 for level sliders
    pub label_w: f32,         // 24.0 for effect sliders, 40.0 for level sliders
    pub label_h: f32,         // 12.0 / 14.0
    pub track: TrackStyle,    // FilledLeft | BalanceGradient
}
```

Fixed geometry for all of them: component 160 × 18, track rect `(8, 7, 112, 3)` with corner radius
5.6, thumb 16 × 16 centred at `(pos, 9)`, focus halo = track-bounds `(8, 0, 112, 18)` expanded by 4
with corner radius 26.

### 13.3 Painting the thumb without an SVG rasteriser

The thumb is simple enough to draw with egui primitives, matching `Slider_Thumb.svg` **(all values
read from `fxsound/Images/Slider_Thumb.svg:6-9,11,27` and `Slider_Thumb_bw.svg:6-9,22`)**:

| Layer | Dark (enabled) | Light (enabled) | Disabled (both) |
|---|---|---|---|
| Body: filled circle r = 8 | linear gradient 0°→135° `#D9304F` → `#DC3253` | `#0A4D66` → `#0D5F7E` | `#818181` → `#9F9F9F` |
| Ring: 1 px stroke at r = 7.5 | gradient `#D52F4E` → `#A41A28` | `#0A4D66` → `#063545` | `#9D9D9D` → `#7B7B7B` |
| Centre dot: filled circle r = 3 | `#0F0F0F` | `#f0f0f0` | `#0F0F0F` |

(egui has no gradient brush; approximate with 2–3 concentric filled shapes or a small cached
texture. The visual difference at 16 px is negligible.) The two colour SVGs declare a 64 × 64
viewBox containing a 16 × 16 glyph while the BW one declares 16 × 16 — the rendering code always
supplies an explicit 16 × 16 destination rect, so the viewBox difference does not matter.

The power glyph is likewise trivial: a ~300° arc of stroke width ≈ 2.6 (scaled from the 30 × 31
viewBox into 24 × 24) plus a vertical bar of the same width from the top down to the centre.

### 13.4 Immediate-mode translation of the JUCE callbacks

| JUCE | egui |
|---|---|
| `resized()` recomputing child bounds | compute rects from the parent `Rect` each frame |
| `paint()` re-applying translated strings | read from the i18n catalogue each frame (free) |
| `valueChanged()` with a `!=` guard | `if response.changed() { … }` |
| `setValue(v, dontSendNotification)` | write the model field directly, skip the change hook |
| `visibilityChanged()` → `update()` | pull from the model when the face is shown |
| `FxModel::Listener` + `MessageManager::callAsync` | an `mpsc`/`crossbeam` channel drained at the top of each frame, or `Arc<RwLock<Model>>` + `ctx.request_repaint()` |
| `TooltipWindow` | `response.on_hover_text(...)`, but reimplement `getTooltipBounds`/`drawTooltip` for the 400 px wrap, 5 px radius and 10 px horizontal padding |

### 13.5 Audio-side mapping (for completeness)

The effect and level controls end at `DfxDsp`, which is a Windows-only static library in this tree.
For the Linux port the equivalent chain is a PipeWire **filter node** (`pw-filter` / `pipewire-rs`)
inserted between a null sink the app creates and the real device, with the five effect strengths and
the four level parameters exposed as its control ports. The power button then maps to
"link/unlink the filter and restore the default sink" — the direct analogue of
`audio_passthru_->restoreDefaultPlaybackDevice()` on power-off. Details belong in the audio spec,
not here; what matters for *this* subsystem is that each control writes a single scalar and that the
write is cheap enough to do on every drag frame (the original writes on every `valueChanged`).

---

## 14. Open questions / risks for the Rust port

1. **The exact track rect depends on JUCE framework code that is not in this repository.**
   §3.1 derives `sliderBounds = (8, 0, 112, 18)` on the assumption that
   `LookAndFeel_V2::getSliderLayout` applies only the `thumbIndent` reduction for a horizontal,
   non-bar, `NoTextBox` slider. If that function also applies a `reduce(1, 1)` in the `NoTextBox`
   case, every derived number shifts to `(9, 1, 110, 16)` — track y 7→7, thumb y 1→1, travel
   110 px instead of 112, value-label x range shifted by 1. **Resolve this by reading
   `modules/juce_gui_basics/lookandfeel/juce_LookAndFeel_V2.cpp` from JUCE 6.1.6 before freezing
   the geometry**, or by screenshotting the Windows build at a known value and measuring.
2. **Global hotkeys cannot be reproduced 1:1 on Wayland.** §9.8 details the portal-based
   substitute. Consequences the product owner must sign off on: the in-app key-capture editor
   disappears; the compositor may refuse or remap the preferred triggers; on compositors without
   `GlobalShortcuts` portal support the feature degrades to "bind this D-Bus method yourself".
   The five default combinations (Ctrl+Shift+Q/E/A/Z/W) should still be *requested* so the
   behaviour matches where the portal honours preferences.
3. **`ToUnicodeEx`-based hotkey validation** has no direct analogue; the `xkbcommon` substitute in
   §9.8(5) is only needed on the X11 fallback path, and its results will differ for layouts where
   AltGr behaves unusually.
4. **Two painting artefacts (§3.4)** — the 8 px filled-track overshoot and the balance gradient
   ending at x = 112 instead of 120 — are almost certainly unintentional. Decide explicitly whether
   the port reproduces them. Recommendation: fix them, and gate the old behaviour behind a
   `--pixel-faithful` debug flag so screenshots can be diffed against the Windows build.
5. **`FxPowerButton::image_width_` is read before it is ever initialised** if `paint()` runs before
   `setImageWidth()` (`FxPowerButton.h:47`). In Rust this is simply a `24.0` constant; flag it if
   anyone tries to "port the bug".
6. **Preset ordering is filesystem-order, not sorted** (§8.4). `FileSearchPath::findChildFiles`
   returns whatever the OS enumerator yields; on Windows NTFS that is usually alphabetical, on
   ext4/btrfs it is hash order. **The Linux port must sort explicitly** or the preset list will
   look random. Recommend: factory presets in a hand-curated order shipped as a manifest, user
   presets sorted case-insensitively by name.
7. **Value-label overflow** (§5.3): the 40 px label can start 5 px outside the 160 px slider. Long
   translations of the unit (`dB`) or wide CJK digits will clip. Clamp and right-align.
8. **Gilroy is a commercial typeface.** It is bundled as a binary resource in the Windows build
   (`FxTheme.cpp:95-97` references `BinaryData::GilroyRegular_ttf` etc.). Verify the licence
   permits redistribution in a Linux package; if not, pick a metrics-compatible substitute
   (Montserrat/Poppins are the usual stand-ins) and re-verify every hard-coded pixel width in this
   document — particularly the 40 px value labels, the 170 px hotkey caption and the
   `width - 37` combo text clip.
9. **`FxAudioSlider`'s value label intercepts mouse clicks** (§5.3) because it is a normal visible
   child that never calls `setInterceptsMouseClicks(false, false)` — unlike the effect sliders'
   label, which does (`FxAudioControls.cpp:190`). In the original, clicking directly on the
   `"0 dB"` text does not move the Master Gain slider. This is a bug; do not reproduce it.
10. **The Master Gain step (2) versus the controller's integer rounding** means half the nominal
    range is unreachable from the UI. Confirm whether the intent was `step = 1`.
11. **`showValues(false)` is dead** (§4.5) but the plumbing survives. Decide whether to keep a
    compact mode or delete the flag.
12. **`FxPresetNameEditor` is dead code** duplicated inline in `FxMainWindow.cpp` (§11.1). Build one
    widget in Rust and use it for both Save and Rename; the only difference in the original is the
    placeholder string and which controller method Return calls.
13. **`FxEqualizerControl` declares four unused constants** (`LABEL_WIDTH = 52`, `CONTROL_GAP = 4`,
    `CONTROL_WIDTH = 100`, plus `FxBalanceSlider::X_MARGIN = 15`) — evidence of an earlier layout.
    Do not try to reverse-engineer meaning from them.
14. **Right-click-to-reset is undiscoverable and undocumented in the UI** (no tooltip mentions it,
    §3.5). Consider adding a visible affordance (a small reset glyph on hover) in the Linux port,
    or at least mention it in the tooltip text.
15. **Theme switching re-creates every `Drawable` from SVG** (`FxPowerButton.cpp:62-67`,
    `FxTheme.cpp:99-102`) and `FxBalanceSlider` re-parses its thumb on *every value change*
    (`FxBalanceSlider.cpp:154-155`). Cache aggressively in Rust; a naive `resvg` call per frame
    would be a measurable regression.
