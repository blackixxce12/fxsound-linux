# 02 — Theme, Colours, Fonts, Localisation

Reverse-engineering spec for the FxSound Windows app (JUCE 6.1.6, `LookAndFeel_V4`),
written for a from-scratch re-implementation in **Rust 1.98.1 + egui/eframe 0.36.0**
(winit / native Wayland, wgpu or glow renderer).

Everything below is taken from lines actually read in the source tree. Every number
carries a `path:line` citation. Paths are relative to the repository root
`/home/blackixxce/Загрузки/fxsound-app-main`.

Primary sources read in full:

| File | Lines |
|---|---|
| `fxsound/Source/GUI/FxTheme.h` | 122 |
| `fxsound/Source/GUI/FxTheme.cpp` | 704 |
| `fxsound/Source/GUI/FxLanguage.h` | 51 |
| `fxsound/Source/GUI/FxLanguage.cpp` | 111 |

Secondary sources consulted for call sites / asset inventory:
`fxsound/JuceLibraryCode/BinaryData.h`, `fxsound/Images/*.svg`, `fxsound/Fonts/*`,
`fxsound/FxSound.jucer`, `fxsound/Source/Main.cpp`,
`fxsound/Source/GUI/FxController.cpp`, `FxWindow.{h,cpp}`, `FxSettingsDialog.{h,cpp}`,
`FxOutputPreference.cpp`, `FxVisualizer.cpp`, `FxBalanceSlider.cpp`, `FxComboBox.cpp`,
`FxEqualizer.{h,cpp}`, `FxAudioControls.cpp`, `FxMainWindow.cpp`, `FxSystemTrayView.cpp`,
`FxPresetExportDialog.cpp`, `FxNotification.cpp`, `FxProView.cpp`,
`fxsound/Source/Utils/Settings/Settings.{h,cpp}`, `fxsound/Project/status_icons.rc`.

---

## 1. Architecture of the theme subsystem

```
                LookAndFeel::setDefaultLookAndFeel(&theme_)        Main.cpp:63
                                   |
                            class FxTheme : public LookAndFeel_V4  FxTheme.h:39
                                   |
    +------------------------------+---------------------------------+
    |                              |                                 |
 static tables               instance state                    virtual overrides
 theme_colors_[2][27]        font_400_ / font_600_ / font_700_  drawComboBox
 theme_images_[2][32]        drop_down_arrow_ (+ _grey_)        drawLinearSlider
 theme_image_sizes_[2][32]   slider_thumb_   (+ _grey_)         drawRotarySlider
 theme_mode_ (Dark|Light)                                       drawPopupMenuItem
                                                                drawTooltip ... etc.
```

* `FxTheme` is a **singleton by convention**: it is a private member `FxTheme theme_;`
  of `FxSoundApplication` (`fxsound/Source/Main.cpp:142`) installed as the JUCE default
  look-and-feel at `fxsound/Source/Main.cpp:63`, and torn down with
  `LookAndFeel::setDefaultLookAndFeel(nullptr)` at `fxsound/Source/Main.cpp:122`.
* All theme state that matters is **`static`**: `theme_mode_`, `theme_colors_`,
  `theme_images_`, `theme_image_sizes_` (`fxsound/Source/GUI/FxTheme.h:110-113`).
* Colour/image lookup goes through three statics
  (`FxTheme.cpp:500-513`) exposed as macros (`FxTheme.h:118-120`):

  ```cpp
  #define FXCOLOR(color)     (FxTheme::getColor(FxColor::color))
  #define FXIMAGE(image)     (FxTheme::getImage(FxImage::image))
  #define FXIMAGESIZE(image) (FxTheme::getImageSize(FxImage::image))
  ```

### 1.1 Theme mode enum and persistence

```cpp
enum FxThemeMode : int { Dark = 0, Light, NumModes };   // FxTheme.h:28
FxThemeMode FxTheme::theme_mode_ = FxThemeMode::Dark;   // FxTheme.cpp:61  (default = Dark)
```

| Fact | Value | Citation |
|---|---|---|
| Default mode at static-init time | `Dark` (0) | `FxTheme.cpp:61` |
| Persisted key | `"theme_mode"` (int) | `FxController.cpp:755`, `FxController.cpp:2768` |
| Load, with clamping | `getInt("theme_mode", 0)`; `< 0 \|\| >= NumModes` → `0` | `FxController.cpp:755-758` |
| Switching re-runs `FxTheme::init()` | yes — rebuilds colours **and** re-decodes the 4 cached SVG drawables | `FxTheme.cpp:491-498` |
| Switching also reloads fonts | `setLanguage(getLanguage())` "To reload font for the new theme" | `FxController.cpp:2769` |
| Switching repaints tray + window icon | `main_window_->setIcon(...)`, `system_tray_view_->setStatus(...)` | `FxController.cpp:2772-2774` |
| No "follow system theme" option exists | only `Dark` / `Light` menu items | `FxMainWindow.cpp:515-516`, `FxSystemTrayView.cpp:289-290` |

Settings are a JUCE `PropertiesFile`: application name `"FxSound"`, folder `"FxSound"`,
extensions `"settings"` (plain) and `"secure"`
(`fxsound/Source/Utils/Settings/Settings.h:29-32`, `Settings.cpp:42-49`).
On Windows that lands in `%APPDATA%\FxSound\FxSound.settings`.

**Linux equivalent.** Store theme mode in
`$XDG_CONFIG_HOME/fxsound/settings.toml` (use the `directories` crate →
`ProjectDirs::from("", "", "fxsound").config_dir()`). Add a third mode
`System`, resolved via the XDG desktop portal
`org.freedesktop.portal.Settings.Read("org.freedesktop.appearance", "color-scheme")`
(`0 = no preference`, `1 = prefer dark`, `2 = prefer light`) and subscribe to
`SettingChanged` so the app re-themes live. `dark-light` crate wraps this if you
do not want to speak D-Bus directly.

---

## 2. Colour system

### 2.1 The `FxColor` enum — canonical order

Declared at `fxsound/Source/GUI/FxTheme.h:29-31`. **The order is load-bearing**: the
two colour rows in `FxTheme.cpp` are positional, not named.

| # | `FxColor` id |
|---|---|
| 0 | `WindowBackground` |
| 1 | `WidgetBackground` |
| 2 | `MenuBackground` |
| 3 | `Outline` |
| 4 | `DefaultText` |
| 5 | `DefaultFill` |
| 6 | `HighlightedText` |
| 7 | `HighlightedFill` |
| 8 | `MenuText` |
| 9 | `ComboBoxBackground` |
| 10 | `TextButtonBackground` |
| 11 | `ImageButton` |
| 12 | `HintText` |
| 13 | `ValidTextBorder` |
| 14 | `InvalidTextBorder` |
| 15 | `ControlBackground` |
| 16 | `SliderTrack` |
| 17 | `SliderHighlight` |
| 18 | `GraphHigh` |
| 19 | `GraphLow` |
| 20 | `EqStart` |
| 21 | `EqEnd` |
| 22 | `VerticalSliderLow` |
| 23 | `MenuHighlightBackground` |
| 24 | `PanelBackground` |
| 25 | `RowOutline` |
| 26 | `SelectedRowOutline` |
| 27 | `NumColors` (sentinel) |

### 2.2 CRITICAL: the stored values have **zero alpha**

`theme_colors_` is `const uint32[...]` and every literal is written as a **6-hex-digit
0xRRGGBB** value (`FxTheme.cpp:23-29`), i.e. the top byte (alpha) is `0x00`.
`Colour(uint32)` in JUCE interprets the argument as **ARGB**, so a raw `FXCOLOR(x)` is
fully transparent. That is why *every* call site in the codebase wraps it:

```cpp
Colour(FXCOLOR(ControlBackground)).withAlpha(1.0f)     // FxWindow.cpp:141
Colour(FXCOLOR(SliderTrack)).withAlpha(0.2f)           // FxTheme.cpp:221
```

> **Port rule:** store the palette as `egui::Color32::from_rgb(r, g, b)` (alpha
> implicit 255) and apply opacity explicitly at each use with a helper
> `fn a(c: Color32, f: f32) -> Color32 { Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (f * 255.0).round() as u8) }`.
> Do **not** try to bake alpha into the palette — the same id is used at 0.0, 0.1,
> 0.2, 0.34, 0.4, 0.5 and 1.0 in different places.

### 2.3 Full colour table — Dark and Light

Dark row: `FxTheme.cpp:23-25`. Light row: `FxTheme.cpp:27-29`.

| # | `FxColor` id | Dark hex | Dark RGB | Light hex | Light RGB | Where it is used (with citation) |
|---|---|---|---|---|---|---|
| 0 | `WindowBackground` | `#181818` | 24,24,24 | `#F5F5F5` | 245,245,245 | Colour-scheme slot 1 (`FxTheme.cpp:107`); whole-window fill in `FxWindow::paint` (`FxWindow.cpp:131-139`); `FxProView::paint` background (`FxProView.cpp:111-112`); `FxLiteView.cpp:51`; title-bar fill (`FxWindow.cpp:239`); close-button background (`FxWindow.cpp:157`) |
| 1 | `WidgetBackground` | `#181818` | 24,24,24 | `#F5F5F5` | 245,245,245 | Colour-scheme slot 2 (`FxTheme.cpp:107`); `drawDocumentWindowTitleBar` fill (`FxTheme.cpp:552`); document-window button background (`FxTheme.cpp:606`); output-preference row list backgrounds (`FxOutputPreference.cpp:56`, `:364`) |
| 2 | `MenuBackground` | `#383838` | 56,56,56 | `#C7C7C7` | 199,199,199 | Colour-scheme slot 3 (`FxTheme.cpp:107`); un-selected settings side-nav button (`FxSettingsDialog.cpp:56`) |
| 3 | `Outline` | `#2B2B2B` | 43,43,43 | `#FAFAFA` | 250,250,250 | Colour-scheme slot 4 (`FxTheme.cpp:108`); vertical separator line in the settings dialog (`FxSettingsDialog.cpp:42-43`) |
| 4 | `DefaultText` | `#B1B1B1` | 177,177,177 | `#4E4E4E` | 78,78,78 | Colour-scheme slot 5 (`FxTheme.cpp:108`); `ComboBox::textColourId` (`FxTheme.cpp:77`); `TextEditor::textColourId` (`:81`); `CaretComponent::caretColourId` (`:88`); export/import list text (`FxPresetExportDialog.cpp:85`, `FxPresetImportDialog.cpp:211,213`); un-selected settings nav label (`FxSettingsDialog.cpp:69`) |
| 5 | `DefaultFill` | `#000000` | 0,0,0 | `#FFFFFF` | 255,255,255 | Colour-scheme slot 6 **at α 0.2** (`FxTheme.cpp:108`); `TextEditor` background/outline/focused-outline (`FxTheme.cpp:78-80`); `PopupMenu::backgroundColourId` (`:89`); the three document-window button glyph colours (`:645,652,669`); notification card fill (`FxNotification.cpp:210`); help-bubble background (`FxMainWindow.cpp:341`); preset-editor background at α **0.0** (`FxMainWindow.cpp:57`); combo-box "no error" outline (`FxComboBox.cpp:71`); export ListBox bg/outline (`FxPresetExportDialog.cpp:83-84`) |
| 6 | `HighlightedText` | `#FFFFFF` | 255,255,255 | `#000000` | 0,0,0 | Colour-scheme slot 7 (`FxTheme.cpp:109`); `TextEditor::highlightedTextColourId` (`:82`); `TextButton::textColourOffId`/`textColourOnId` (`:85-86`); `HyperlinkButton::textColourId` (`:87`); selected settings nav label (`FxSettingsDialog.cpp:65`); export list row text (`FxPresetExportDialog.cpp:153`); title-bar title label (`FxWindow.cpp:383`) |
| 7 | `HighlightedFill` | `#0C0C0C` | 12,12,12 | `#E0E0E0` | 224,224,224 | Colour-scheme slot 8 only (`FxTheme.cpp:109`) |
| 8 | `MenuText` | `#FFFFFF` | 255,255,255 | `#000000` | 0,0,0 | Colour-scheme slot 9 only (`FxTheme.cpp:109`) |
| 9 | `ComboBoxBackground` | `#000000` | 0,0,0 | `#D7D7D7` | 215,215,215 | `ComboBox::backgroundColourId` **and** `ComboBox::outlineColourId` (`FxTheme.cpp:74-75`) — the outline is the same colour as the fill, i.e. invisible by default |
| 10 | `TextButtonBackground` | `#D51535` | 213,21,53 | `#1AC1FF` | 26,193,255 | `TextButton::buttonColourId` and `buttonOnColourId` (`FxTheme.cpp:83-84`) |
| 11 | `ImageButton` | `#E63462` | 230,52,98 | `#23B6EB` | 35,182,235 | `ComboBox::arrowColourId` (`FxTheme.cpp:73`); `PopupMenu::highlightedBackgroundColourId` (`:90`); window close-button glyph (`FxWindow.cpp:167`); selected export row fill (`FxPresetExportDialog.cpp:147`); export progress gradient start (`:67`) |
| 12 | `HintText` | `#7F7F7F` | 127,127,127 | `#7F7F7F` | 127,127,127 | **Identical in both themes.** Preset-name placeholder label (`FxMainWindow.cpp:41`, `FxPresetNameEditor.cpp:43`) |
| 13 | `ValidTextBorder` | `#009CDD` | 0,156,221 | `#009CDD` | 0,156,221 | **Identical in both themes.** Preset-name editor border when the name is valid (`FxMainWindow.cpp:115`, `FxPresetNameEditor.cpp:76`) |
| 14 | `InvalidTextBorder` | `#D51535` | 213,21,53 | `#D51535` | 213,21,53 | **Identical in both themes.** Preset-name editor border when empty/invalid (`FxMainWindow.cpp:120`, `FxPresetNameEditor.cpp:81`) |
| 15 | `ControlBackground` | `#0F0F0F` | 15,15,15 | `#E0E0E0` | 224,224,224 | Rounded card behind the audio controls, r = 8 (`FxAudioControls.cpp:88-89`); equaliser card, r = 8 (`FxEqualizer.cpp:310-311`); visualiser card, r = 8 (`FxVisualizer.cpp:135-136`); language switcher pill, r = 5 (`FxLanguage.cpp:76-77`); the 1-px rule under the title bar (`FxWindow.cpp:141-142`) |
| 16 | `SliderTrack` | `#E33250` | 227,50,80 | `#0A4D66` | 10,77,102 | `Slider::rotarySliderOutlineColourId` **@ α 0.2** and `rotarySliderFillColourId` **@ α 1.0** (`FxTheme.cpp:91-92`); `ScrollBar::thumbColourId` (`:93`); vertical-slider dash gradient top **@ 0.4** (`:192`); horizontal-slider rail **@ 0.2** and filled part **@ 1.0** (`:221`, `:230`); balance-slider gradient (`FxBalanceSlider.cpp:81-82`); EQ curve stroke (`FxEqualizer.cpp:315`); combo-box error outline (`FxComboBox.cpp:66`) |
| 17 | `SliderHighlight` | `#F7546F` | 247,84,111 | `#53CCFF` | 83,204,255 | `ComboBox::focusedOutlineColourId` **@ α 0.2** (`FxTheme.cpp:76`); keyboard-focus / drag halo on all sliders **@ α 0.1** (`FxTheme.cpp:212`, `:246`, `:304`, `FxBalanceSlider.cpp:101`) |
| 18 | `GraphHigh` | `#D51535` | 213,21,53 | `#1AC1FF` | 26,193,255 | Spectrum-visualiser gradient stops at offsets 0.0 and 1.0 (`FxVisualizer.cpp:185-188`) |
| 19 | `GraphLow` | `#FE566A` | 254,86,106 | `#72D8FF` | 114,216,255 | Spectrum-visualiser gradient mid stop at offset 0.5 (`FxVisualizer.cpp:191`, `:195`) |
| 20 | `EqStart` | `#EF4B65` | 239,75,101 | `#33C8FF` | 51,200,255 | EQ fill gradient start **@ α 0.34** (`FxEqualizer.cpp:316`) |
| 21 | `EqEnd` | `#742834` | 116,40,52 | `#063244` | 6,50,68 | EQ fill gradient end **@ α 0.0** (`FxEqualizer.cpp:317`) — fully transparent, so the hue never actually shows; only the α ramp matters |
| 22 | `VerticalSliderLow` | `#F3F3F3` | 243,243,243 | `#1C1C1C` | 28,28,28 | Bottom colour of the vertical EQ-slider dashed rail gradient **@ α 0.4** (`FxTheme.cpp:194`); export-progress gradient end **@ α 1.0** (`FxPresetExportDialog.cpp:67`) |
| 23 | `MenuHighlightBackground` | `#414141` | 65,65,65 | `#B9B9B9` | 185,185,185 | Selected settings side-nav button fill (`FxSettingsDialog.cpp:52`) |
| 24 | `PanelBackground` | `#000000` | 0,0,0 | `#C0C0C0` | 192,192,192 | Pro-view inner panel **@ α 0.2**, rect `(20, 16, 1000, 347 + visualizer_offset)`, r = 8 (`FxProView.cpp:114-115`) |
| 25 | `RowOutline` | `#B1B1B1` | 177,177,177 | `#4E4E4E` | 78,78,78 | Output-device row underline **@ α 1.0**, 0.5 px (`FxOutputPreference.cpp:192-193`); row combo outline **@ α 1.0** (`:57`) and **@ α 0.5** when unselected (`:156`) |
| 26 | `SelectedRowOutline` | `#E63462` | 230,52,98 | `#23B6EB` | 35,182,235 | Output-device row underline when selected, 1.0 px (`FxOutputPreference.cpp:187-188`); focused/selected row combo outline (`:58`, `:150`) |

Observations worth carrying into the port:

* `WindowBackground` and `WidgetBackground` are **identical** within each theme.
* `HintText`, `ValidTextBorder`, `InvalidTextBorder` are **theme-invariant**.
  `InvalidTextBorder` (`#D51535`) equals the dark theme's `TextButtonBackground`.
* Dark theme is a red/crimson accent family; light theme is a cyan/azure family.
  Dark accent: `#E63462` (buttons/arrows), `#D51535` (fills), `#E33250` (tracks).
  Light accent: `#23B6EB` (buttons/arrows), `#1AC1FF` (fills), `#0A4D66` (tracks).
* Light-theme `Outline` (`#FAFAFA`) is *lighter* than light-theme `WindowBackground`
  (`#F5F5F5`) — the settings-dialog separator is effectively invisible in light mode.
  Reproduce faithfully or file it as an intentional deviation.

### 2.4 The JUCE `LookAndFeel_V4::ColourScheme` (9 slots)

`FxTheme::getFxColourScheme()` (`FxTheme.cpp:105-110`) builds the 9-slot scheme in this
exact positional order (this is JUCE's `ColourScheme` ctor order):

| Slot | `ColourScheme::UIColour` | Source `FxColor` | Alpha applied |
|---|---|---|---|
| 1 | `windowBackground` | `WindowBackground` | 1.0 |
| 2 | `widgetBackground` | `WidgetBackground` | 1.0 |
| 3 | `menuBackground` | `MenuBackground` | 1.0 |
| 4 | `outline` | `Outline` | 1.0 |
| 5 | `defaultText` | `DefaultText` | 1.0 |
| 6 | `defaultFill` | `DefaultFill` | **0.2** |
| 7 | `highlightedText` | `HighlightedText` | 1.0 |
| 8 | `highlightedFill` | `HighlightedFill` | 1.0 |
| 9 | `menuText` | `MenuText` | 1.0 |

`setColourScheme(...)` is invoked first in `init()` (`FxTheme.cpp:71`) — JUCE then
derives ~150 component colour ids from these 9. The explicit `setColour` calls that
follow (`FxTheme.cpp:73-93`) override 21 of them.

### 2.5 Explicit JUCE colour-id overrides (`FxTheme::init`)

Complete list, in source order, `FxTheme.cpp:73-93`:

| JUCE colour id | Value | α | Line |
|---|---|---|---|
| `ComboBox::arrowColourId` | `ImageButton` | 1.0 | 73 |
| `ComboBox::backgroundColourId` | `ComboBoxBackground` | 1.0 | 74 |
| `ComboBox::outlineColourId` | `ComboBoxBackground` | 1.0 | 75 |
| `ComboBox::focusedOutlineColourId` | `SliderHighlight` | **0.2** | 76 |
| `ComboBox::textColourId` | `DefaultText` | 1.0 | 77 |
| `TextEditor::backgroundColourId` | `DefaultFill` | 1.0 | 78 |
| `TextEditor::outlineColourId` | `DefaultFill` | 1.0 | 79 |
| `TextEditor::focusedOutlineColourId` | `DefaultFill` | 1.0 | 80 |
| `TextEditor::textColourId` | `DefaultText` | 1.0 | 81 |
| `TextEditor::highlightedTextColourId` | `HighlightedText` | 1.0 | 82 |
| `TextButton::buttonColourId` | `TextButtonBackground` | 1.0 | 83 |
| `TextButton::buttonOnColourId` | `TextButtonBackground` | 1.0 | 84 |
| `TextButton::textColourOffId` | `HighlightedText` | 1.0 | 85 |
| `TextButton::textColourOnId` | `HighlightedText` | 1.0 | 86 |
| `HyperlinkButton::textColourId` | `HighlightedText` | 1.0 | 87 |
| `CaretComponent::caretColourId` | `DefaultText` | 1.0 | 88 |
| `PopupMenu::backgroundColourId` | `DefaultFill` | 1.0 | 89 |
| `PopupMenu::highlightedBackgroundColourId` | `ImageButton` | 1.0 | 90 |
| `Slider::rotarySliderOutlineColourId` | `SliderTrack` | **0.2** | 91 |
| `Slider::rotarySliderFillColourId` | `SliderTrack` | 1.0 | 92 |
| `ScrollBar::thumbColourId` | `SliderTrack` | 1.0 | 93 |

Notably **not** overridden, therefore inherited from JUCE's own derivation of the
9-slot scheme: `TooltipWindow::{background,text,outline}ColourId`,
`PopupMenu::{text,highlightedText}ColourId`, `Label::textColourId`,
`ListBox::*`, `ScrollBar::{background,track}ColourId`.
`TooltipWindow::textColourId` is then explicitly re-set per-instance to
`defaultText` in `FxProView.cpp:39`.

### 2.6 Mapping the palette onto `egui::Visuals`

Build two `Visuals` values from one palette struct. Target fields for egui 0.36:

```rust
pub struct Palette {
    pub window_background: Color32,   pub widget_background: Color32,
    pub menu_background: Color32,     pub outline: Color32,
    pub default_text: Color32,        pub default_fill: Color32,
    pub highlighted_text: Color32,    pub highlighted_fill: Color32,
    pub menu_text: Color32,           pub combo_box_background: Color32,
    pub text_button_background: Color32, pub image_button: Color32,
    pub hint_text: Color32,           pub valid_text_border: Color32,
    pub invalid_text_border: Color32, pub control_background: Color32,
    pub slider_track: Color32,        pub slider_highlight: Color32,
    pub graph_high: Color32,          pub graph_low: Color32,
    pub eq_start: Color32,            pub eq_end: Color32,
    pub vertical_slider_low: Color32, pub menu_highlight_background: Color32,
    pub panel_background: Color32,    pub row_outline: Color32,
    pub selected_row_outline: Color32,
}
```

| egui field | Set from | Rationale / citation |
|---|---|---|
| `Visuals::dark_mode` | `mode == Dark` | drives egui's own fallbacks |
| `Visuals::panel_fill` | `window_background` | `FxWindow.cpp:131`, `FxProView.cpp:111` |
| `Visuals::window_fill` | `window_background` | same |
| `Visuals::window_stroke` | `Stroke::NONE` | FxWindow draws no border, only a shadow (`FxWindow.cpp:124-129`) |
| `Visuals::window_corner_radius` | `CornerRadius::same(21)` | `FxTheme.h:42` `WINDOW_CORNER_RADIUS = 21` |
| `Visuals::window_shadow` | `Shadow { offset: [0,0], blur: 5, spread: 0, color: black_alpha }` | `FxWindow.h:44` `SHADOW_WIDTH = 5`; `FxWindow.cpp:127` `shadow.radius = shadow_width_` |
| `Visuals::popup_shadow` | `Shadow { offset: [0,0], blur: 5, spread: 0, .. }` | `FxNotification.cpp:207` `shadow.radius = 5` |
| `Visuals::override_text_color` | **leave `None`** | text colour is per-widget here; forcing one breaks selected-row contrast |
| `Visuals::extreme_bg_color` | `default_fill` | text-editor background (`FxTheme.cpp:78`) |
| `Visuals::faint_bg_color` | `control_background` | the r=8 cards (`FxAudioControls.cpp:88`) |
| `Visuals::selection.bg_fill` | `image_button` | `PopupMenu::highlightedBackgroundColourId` (`FxTheme.cpp:90`) |
| `Visuals::selection.stroke` | `Stroke::new(1.0, highlighted_text)` | ticked-item outline (`FxTheme.cpp:362-363`) |
| `Visuals::hyperlink_color` | `highlighted_text` | `FxTheme.cpp:87` |
| `Visuals::error_fg_color` | `invalid_text_border` (`#D51535`) | `FxMainWindow.cpp:120` |
| `Visuals::widgets.noninteractive.bg_fill` | `window_background` | |
| `Visuals::widgets.noninteractive.fg_stroke` | `Stroke::new(1.0, default_text)` | `FxTheme.cpp:77` |
| `Visuals::widgets.noninteractive.bg_stroke` | `Stroke::new(1.0, outline)` | slot 4 |
| `Visuals::widgets.inactive.bg_fill` / `weak_bg_fill` | `text_button_background` | `FxTheme.cpp:83` |
| `Visuals::widgets.inactive.fg_stroke` | `Stroke::new(1.0, highlighted_text)` | `FxTheme.cpp:85` |
| `Visuals::widgets.hovered.bg_fill` | `image_button` | hover art is the `*_hover` SVG family |
| `Visuals::widgets.active.bg_fill` | `image_button` | |
| `Visuals::widgets.*.corner_radius` | see §5 per-control | not one global value |
| `Visuals::menu_corner_radius` | `CornerRadius::same(5)` | tooltip/menu radius (`FxTheme.cpp:531`) |
| `Visuals::slider_trailing_fill` | `true` | horizontal sliders fill up to the thumb (`FxTheme.cpp:236-237`) |
| `Visuals::handle_shape` | `HandleShape::Circle` | thumb is a circle (`Slider_Thumb.svg` `path-3` is a 16×16 circle) |
| `Style::spacing.scroll.bar_width` | `10.0` | `FxPresetImportDialog.cpp:59`, `:81` |

Colours with **no** `Visuals` slot (`graph_*`, `eq_*`, `vertical_slider_low`,
`panel_background`, `row_outline`, `selected_row_outline`, `hint_text`,
`valid_text_border`, `slider_highlight`, `control_background`,
`menu_highlight_background`) must live in your own `Palette` in
`egui::Context::data` / `Memory::data` (or a plain resource in your app struct) and be
read by the custom painters in §5.

### 2.7 The "disabled" transform

Disabled controls do **not** get a separate palette. They desaturate the live colour:

```cpp
colour1 = colour1.withSaturation(0.0);      // FxTheme.cpp:198, 199, 224, 233, 265, 266
```

`juce::Colour::withSaturation(0.0f)` is **HSB** saturation → 0, preserving hue-free
brightness. Equivalent Rust helper:

```rust
fn desaturate(c: Color32) -> Color32 {
    let (_h, _s, v) = ecolor::Hsva::from(c).to_tuple_hsv_ignoring_alpha(); // or hand-rolled
    let g = (v * 255.0).round() as u8;
    Color32::from_rgba_unmultiplied(g, g, g, c.a())
}
```
Note this is **max(r,g,b)** grey, not luminance grey — a 0.299/0.587/0.114 luma
conversion will look visibly different. Same call sites appear in
`FxEqualizer.cpp:321-323`, `FxVisualizer.cpp:185-195`, `FxBalanceSlider.cpp:85-86`.

---

## 3. Fonts

### 3.1 Files on disk (`fxsound/Fonts/`)

| File | Bytes | Used for |
|---|---|---|
| `Gilroy-Regular.ttf` | 84300 | weight 400, Latin/default |
| `Gilroy-Semibold.ttf` | 83948 | weight 600, Latin/default |
| `Gilroy-Bold.ttf` | 83456 | weight 700, Latin/default |
| `NotoSansKR-Regular.otf` | 4744692 | Korean 400 |
| `NotoSansKR-Medium.otf` | 4768768 | Korean 600 and 700 |
| `NotoSansKR-Bold.otf` | 4909668 | **present but never referenced** |
| `NotoSansSC-Regular.otf` | 8482020 | Simplified Chinese 400 |
| `NotoSansSC-Medium.otf` | 8508580 | Simplified Chinese 600 and 700 |
| `NotoSansSC-Bold.otf` | 8716392 | **present but never referenced** |
| `NotoSansTC-Regular.otf` | 5766468 | Traditional Chinese — see bug below |
| `NotoSansTC-Medium.otf` | 5788004 | Traditional Chinese — see bug below |
| `NotoSansTC-Bold.otf` | 5942628 | **present but never referenced** |
| `NotoSansThai-Regular.ttf` | 47404 | Thai 400 |
| `NotoSansThai-Medium.ttf` | 47472 | Thai 600 and 700 |
| `NotoSansArabic-Regular.ttf` | 177004 | **present but never referenced** |
| `NotoSansArabic-Medium.ttf` | 177576 | **present but never referenced** |

### 3.2 Embedding vs. runtime loading

Only the three Gilroy faces are compiled into the binary
(`fxsound/FxSound.jucer:10-14` marks exactly those three `resource="1"`, and
`fxsound/JuceLibraryCode/BinaryData.h:11-18` declares only those three):

| BinaryData symbol | Size const | Value |
|---|---|---|
| `GilroyBold_ttf` | `GilroyBold_ttfSize` | `83456` (`BinaryData.h:12`) |
| `GilroyRegular_ttf` | `GilroyRegular_ttfSize` | `84300` (`BinaryData.h:15`) |
| `GilroySemibold_ttf` | `GilroySemibold_ttfSize` | `83948` (`BinaryData.h:18`) |

Everything else is loaded from **the process working directory** at runtime:

```cpp
Typeface::Ptr FxTheme::loadTypeface(String fileName)              // FxTheme.cpp:691-704
{
    MemoryBlock fontBuffer;
    String filePath = File::addTrailingSeparator(File::getCurrentWorkingDirectory().getFullPathName());
    File fontFile = File(filePath + fileName);
    if (fontFile.exists() && fontFile.loadFileAsData(fontBuffer))
        return Typeface::createSystemTypefaceFor(fontBuffer.getData(), fontBuffer.getSize());
    return nullptr;                                                // silent fallback
}
```

The CWD is forced to the executable's own directory at startup
(`fxsound/Source/Main.cpp:308-321`, `GetModuleFileName` + `SetCurrentDirectory`),
so in practice these are files shipped next to `fxsound.exe`.

### 3.3 Per-language font selection — `FxTheme::loadFont(String language)`

`FxTheme.cpp:382-459`. Dispatch is a chain of `startsWithIgnoreCase` tests against
the **BCP-47-ish language code** (not the display name). Order matters.

| Test (`startsWithIgnoreCase`) | `font_400_` | `font_600_` | `font_700_` | Lines |
|---|---|---|---|---|
| `"en"` | `BinaryData::GilroyRegular_ttf` | `GilroySemibold_ttf` | `GilroyBold_ttf` | 384-389 |
| `"ko"` | `NotoSansKR-Regular.otf` | `NotoSansKR-Medium.otf` | `NotoSansKR-Medium.otf` | 390-395 |
| `"zh-CN"` | `NotoSansSC-Regular.otf` | `NotoSansSC-Medium.otf` | `NotoSansSC-Medium.otf` | 396-401 |
| `"zh-TW"` | `NotoSansTC-Regular.ttf` **(.ttf)** | `NotoSansTC-Medium.ttf` **(.ttf)** | `NotoSansTC-Medium.ttf` | 402-407 |
| `"th"` | `NotoSansThai-Regular.ttf` | `NotoSansThai-Medium.ttf` | `NotoSansThai-Medium.ttf` | 408-413 |
| `"vi"` | `MontserratAlternates-Regular.ttf` | `MontserratAlternates-Medium.ttf` | `MontserratAlternates-Bold.ttf` | 414-419 |
| `"ja"` | `NotoSansJP-Regular.ttf` | `NotoSansJP-Medium.ttf` | `NotoSansJP-Bold.ttf` | 420-425 |
| `"ar"` | `IBMPlexSansArabic-Regular.ttf` | `IBMPlexSansArabic-Medium.ttf` | `IBMPlexSansArabic-Bold.ttf` | 426-431 |
| `"fa"` | `IBMPlexSansArabic-Regular.ttf` | `IBMPlexSansArabic-Medium.ttf` | `IBMPlexSansArabic-Bold.ttf` | 432-437 |
| *anything else* | Gilroy trio | Gilroy trio | Gilroy trio | 438-443 |

Then, per-slot null-guards (`FxTheme.cpp:445-456`): any face that came back `nullptr`
falls back to the corresponding Gilroy binary resource. Finally
`setDefaultSansSerifTypeface(font_600_)` (`FxTheme.cpp:458`) — **Semibold is the
process-wide default sans-serif**, which is what `Font(12.0f, Font::plain)` in
`drawDocumentWindowTitleBar` (`FxTheme.cpp:555`) resolves to.

#### Defects in this table that the port must decide about

1. **`zh-TW` asks for `.ttf`, the shipped files are `.otf`.**
   `FxTheme.cpp:404-406` requests `NotoSansTC-Regular.ttf` / `NotoSansTC-Medium.ttf`;
   `fxsound/Fonts/` contains `NotoSansTC-Regular.otf` / `NotoSansTC-Medium.otf`.
   `loadTypeface` returns `nullptr` → silent Gilroy fallback → Traditional Chinese
   renders with a Latin-only face (tofu). Fix in the port.
2. **`vi`, `ja`, `ar`, `fa` reference fonts that do not exist in this tree at all**
   (`MontserratAlternates-*`, `NotoSansJP-*`, `IBMPlexSansArabic-*`). They may be
   shipped by the installer, but from this source tree all four fall back to Gilroy.
3. **`NotoSansArabic-{Regular,Medium}.ttf` are shipped but never referenced** — the
   code asks for IBM Plex Sans Arabic instead. Either wire Noto up or drop the files.
4. **`NotoSans{KR,SC,TC}-Bold.otf` are shipped but never referenced** — weight 700 for
   CJK reuses the Medium face.
5. `zh-CN` / `zh-TW` are tested **before** any bare `zh`, but the else-branch means a
   plain `"zh"` locale gets Gilroy, not SC. Worth normalising in the port.

### 3.4 The three named font roles and their sizes

| API | Face | Height (px) | Citation |
|---|---|---|---|
| `getSmallFont()` | `font_400_` (Regular) | **14.0** | `FxTheme.cpp:471-474` |
| `getNormalFont()` | `font_600_` (Semibold) | **17.0** | `FxTheme.cpp:466-469` |
| `getTitleFont()` | `font_700_` (Bold) | **17.0** | `FxTheme.cpp:476-479` |
| `getDefaultTypeface()` | `font_400_` | n/a | `FxTheme.cpp:481-484` |
| `getComboBoxFont(box)` | `font_600_` | **14.0** if `box.getHeight() <= 30`, else **17.0** | `FxTheme.cpp:120-126` |
| `getPopupMenuFont()` | `font_600_` | **17.0** | `FxTheme.cpp:367-370` |
| `getTextButtonFont(_, h)` | `font_600_` | `min(17.0, h)` | `FxTheme.cpp:461-464` |
| tooltip text | `getNormalFont().withHeight(14.0f)` → `font_600_` @ 14 | **14.0** | `FxTheme.cpp:678`, `:684` |
| `drawDocumentWindowTitleBar` | default sans (= `font_600_`) | **12.0**, `Font::plain` | `FxTheme.cpp:555` |

JUCE `Font` "height" is the **full em box height in pixels**, not the point size and
not the cap height. egui `FontId::size` is likewise a pixel size, and epaint's
`FontImpl` scales by `size / units_per_em`, so the two are close enough to copy the
numbers directly — but expect a small ascent/descent mismatch (see §9).

### 3.5 Every distinct font size used in the UI

| px | Face role | Elements | Citation |
|---|---|---|---|
| 10 | Normal (600) | EQ band frequency sub-labels (`SMALL_FONT`) | `FxEqualizer.h:100`, `FxEqualizer.cpp:274` |
| 12 | Normal (600) | slider value read-outs; EQ band labels (`LABEL_HEIGHT`); balance L/R labels; document-window title bar | `FxAudioSlider.cpp:35`, `FxAudioControls.cpp:188,365,372,485,488`, `FxBalanceSlider.cpp:41`, `FxEqualizer.h:65,99`, `FxEqualizer.cpp:267,404`, `FxTheme.cpp:555` |
| 14 | Small (400) | hotkey labels; menu-help bubble; combo boxes ≤ 30 px tall; tooltips | `FxHotkeyLabel.cpp:27,50,66`, `FxMainWindow.cpp:597`, `FxTheme.cpp:123`, `FxTheme.cpp:678` |
| 14 | Normal (600) | effect-slider captions; "Master Gain"/"Volume Leveling"/"Filter Q"/"Balance" section titles | `FxAudioControls.cpp:104,167,305,323,341,359,471` |
| 17 | Normal (600) | default body text everywhere: labels, combo boxes > 30 px, popup-menu items, text buttons, settings rows, window title, language switcher | `FxTheme.cpp:125,369,463,468`, `FxLanguage.cpp:91,108`, `FxWindow.cpp:242,380`, `FxSettingsDialog.cpp:74,280,540,546,549` |
| 17 | Small (400) → forced to 17 | notification body, message dialogs | `FxMessage.cpp:60`, `FxNotification.cpp:80,166` |
| 17 | Title (700) | settings pane titles | `FxSettingsDialog.cpp:170,181` |

### 3.6 egui `FontDefinitions` mapping

```rust
use egui::{FontData, FontDefinitions, FontFamily, FontId, TextStyle};
use std::sync::Arc;

// One family per weight. epaint has no weight axis, so weights become *families*.
const F400: &str = "fx-400";
const F600: &str = "fx-600";
const F700: &str = "fx-700";

fn font_defs(lang: &str) -> FontDefinitions {
    let mut d = FontDefinitions::empty();

    // Always-present Latin faces (embedded, mirrors BinaryData).
    d.font_data.insert(F400.into(), Arc::new(FontData::from_static(GILROY_REGULAR)));
    d.font_data.insert(F600.into(), Arc::new(FontData::from_static(GILROY_SEMIBOLD)));
    d.font_data.insert(F700.into(), Arc::new(FontData::from_static(GILROY_BOLD)));

    // Script face loaded from $XDG_DATA_DIRS/fxsound/fonts (FxTheme::loadTypeface analogue).
    let script: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = script_faces_for(lang);
    if let Some((r, m, b)) = script {
        d.font_data.insert("fx-script-400".into(), Arc::new(FontData::from_owned(r)));
        d.font_data.insert("fx-script-600".into(), Arc::new(FontData::from_owned(m)));
        d.font_data.insert("fx-script-700".into(), Arc::new(FontData::from_owned(b)));
    }

    // FALLBACK CHAINS: unlike JUCE (which *replaces* the face), egui supports a list.
    // Put the script face FIRST so CJK/Thai/Arabic wins, Gilroy second for Latin digits.
    for (fam, latin, script_key) in [
        (F400, F400, "fx-script-400"),
        (F600, F600, "fx-script-600"),
        (F700, F700, "fx-script-700"),
    ] {
        let mut chain = Vec::new();
        if d.font_data.contains_key(script_key) { chain.push(script_key.to_owned()); }
        chain.push(latin.to_owned());
        d.families.insert(FontFamily::Name(fam.into()), chain);
    }
    d.families.insert(FontFamily::Proportional, vec![F600.into(), F400.into()]);
    d.families.insert(FontFamily::Monospace,   vec![F400.into()]);
    d
}
```

`Style::text_styles` seeded from §3.5:

| `TextStyle` | `FontId` |
|---|---|
| `TextStyle::Name("tiny")` | `FontId::new(10.0, FontFamily::Name(F600))` |
| `TextStyle::Small` | `FontId::new(12.0, FontFamily::Name(F600))` |
| `TextStyle::Name("caption")` | `FontId::new(14.0, FontFamily::Name(F400))` |
| `TextStyle::Name("caption-strong")` | `FontId::new(14.0, FontFamily::Name(F600))` |
| `TextStyle::Body` | `FontId::new(17.0, FontFamily::Name(F600))` |
| `TextStyle::Button` | `FontId::new(17.0, FontFamily::Name(F600))` |
| `TextStyle::Heading` | `FontId::new(17.0, FontFamily::Name(F700))` |

**Key divergence to exploit:** JUCE swaps the whole typeface per language, so a
Korean build renders Latin text in Noto Sans KR. egui's per-family *fallback list*
lets you keep Gilroy for Latin and fall through to Noto only for the CJK/Thai
codepoints. This is strictly better and costs nothing — but the metrics (line height,
17 px cap height) will then differ slightly between the two products. Accept it.

**Memory note:** `NotoSansSC-Regular.otf` alone is 8.4 MB; the CJK trio is ~22 MB.
Do **not** `include_bytes!` those. Ship them as separate files in
`/usr/share/fxsound/fonts/` (or depend on the distro's `noto-fonts-cjk`), load lazily
on language change, and call `ctx.set_fonts(...)` — egui rebuilds the atlas on that
call, which is a visible one-frame hitch. FxSound does the same
(`FxController.cpp:2462` then `sendLookAndFeelChange()` at `:2467`).

---

## 4. Image / SVG assets

### 4.1 The `FxImage` enum — canonical order

`fxsound/Source/GUI/FxTheme.h:32-37`.

| # | `FxImage` id | # | `FxImage` id |
|---|---|---|---|
| 0 | `DefaultLogo` | 16 | `FlipButtonHover` |
| 1 | `HighlightedLogo` | 17 | `RestoreDefaultsButton` |
| 2 | `IconLogo` | 18 | `RestoreDefaultsButtonHover` |
| 3 | `PowerOnButton` | 19 | `RemoveButton` |
| 4 | `PowerOffButton` | 20 | `ArrowNext` |
| 5 | `DonateButton` | 21 | `ArrowNextBW` |
| 6 | `DonateButtonHover` | 22 | `ArrowPrev` |
| 7 | `MenuButton` | 23 | `ArrowPrevBW` |
| 8 | `MenuButtonHover` | 24 | `ArrowUpSelected` |
| 9 | `MinimizeButton` | 25 | `ArrowUp` |
| 10 | `MinimizeButtonHover` | 26 | `ArrowDownSelected` |
| 11 | `MaximizeButton` | 27 | `ArrowDown` |
| 12 | `MaximizeButtonHover` | 28 | `DropDownArrow` |
| 13 | `MinimizeWindowButton` | 29 | `DropDownArrowHover` |
| 14 | `MinimizeWindowButtonHover` | 30 | `SliderThumb` |
| 15 | `FlipButton` | 31 | `SliderThumbBW` |
| | | 32 | `NumImages` (sentinel) |

### 4.2 Per-theme SVG table

Dark row: `FxTheme.cpp:32-37`. Light row: `FxTheme.cpp:39-44`.
Byte sizes: `fxsound/JuceLibraryCode/BinaryData.h`. Intrinsic geometry and ink colour
read from `fxsound/Images/*.svg`.

| `FxImage` | Dark SVG | bytes | Light SVG | bytes | intrinsic W×H | Dark ink | Light ink |
|---|---|---|---|---|---|---|---|
| `DefaultLogo` | `logo-white.svg` | 7254 | `logo-black.svg` | 7254 | 13.48×27.36, viewBox `0 0 526.19 75.15` | white | black |
| `HighlightedLogo` | `logo-red.svg` | 7257 | `logo-blue.svg` | 7257 | same | red | blue |
| `IconLogo` | `FxSound White Bars.svg` | 589 | `FxSound Black Bars.svg` | 589 | 43.22×87.75, viewBox `0 0 299.83 219.26` | white | black |
| `PowerOnButton` | `power_on.svg` | 2218 | `power_on_blue.svg` | 2218 | 30×31 | `#E63462` | `#23B6EB` |
| `PowerOffButton` | `power_off.svg` | 2218 | `power_off_black.svg` | 2218 | 30×31 | `#FFFFFF` | `#000000` |
| `DonateButton` | `donate.svg` | 431 | `donate_blue.svg` | 431 | 30×31 | stroke `#E63462` | stroke `#23B6EB` |
| `DonateButtonHover` | `donate_hover.svg` | 431 | `donate_hover_blue.svg` | 431 | 30×31 | stroke `#E63462` | stroke `#23B6EB` |
| `MenuButton` | `menu.svg` | 552 | `menu_black.svg` | 552 | 14×10 | stroke `#FFFFFF` | stroke `#000000` |
| `MenuButtonHover` | `menu_hover.svg` | 669 | `menu_hover_blue.svg` | 669 | 14×10 | `#E63462` | `#23B6EB` |
| `MinimizeButton` | `minimize.svg` | 951 | `minimize_black.svg` | 951 | 18×18 | `#FFFFFF` | `#000000` |
| `MinimizeButtonHover` | `minimize_hover.svg` | 963 | `minimize_hover_blue.svg` | 963 | 18×18 | `#E63462` | `#23b6eb` |
| `MaximizeButton` | `maximize.svg` | 826 | `maximize_black.svg` | 826 | 16×16 | `#FFFFFF` | `#000000` |
| `MaximizeButtonHover` | `maximize_hover.svg` | 838 | `maximize_hover_blue.svg` | 838 | 16×16 | `#E63462` | `#23B6EB` |
| `MinimizeWindowButton` | `min_window.svg` | 542 | `min_window_black.svg` | 542 | 14×10 | stroke `#FFFFFF` | stroke `#000000` |
| `MinimizeWindowButtonHover` | `min_window_hover.svg` | 599 | `min_window_hover_blue.svg` | 599 | 14×10 | `#E63462` | `#23B6EB` |
| `FlipButton` | `flip_white.svg` | 597 | `flip_black.svg` | 597 | 16×16 | stroke `#FFFFFF` | stroke `#000000` |
| `FlipButtonHover` | `flip.svg` | 585 | `flip_blue.svg` | 597 | 16×16 | stroke `#E63462` | stroke `#23B6EB` |
| `RestoreDefaultsButton` | `restore_defaults_white.svg` | 605 | `restore_defaults_black.svg` | 605 | 16×16 | stroke `#FFFFFF` | stroke `#000000` |
| `RestoreDefaultsButtonHover` | `restore_defaults.svg` | 593 | `restore_defaults_blue.svg` | 603 | 16×16 | stroke `#E63462` | stroke `#23B6EB` |
| `RemoveButton` | `remove.svg` | 269 | **`remove.svg`** | 269 | 16×16 | stroke `#D51535` | stroke `#D51535` |
| `ArrowNext` | `arrow_next.svg` | 507 | `arrow_next_blue.svg` | 507 | 7×11 | `#E63462` | `#23B6EB` |
| `ArrowNextBW` | `arrow_next_bw.svg` | 644 | **`arrow_next_bw.svg`** | 644 | 7×11 | stroke `#B0B0B0` | stroke `#B0B0B0` |
| `ArrowPrev` | `arrow_prev.svg` | 506 | `arrow_prev_blue.svg` | 506 | 7×11 | `#E63462` | `#23B6EB` |
| `ArrowPrevBW` | `arrow_prev_bw.svg` | 639 | **`arrow_prev_bw.svg`** | 639 | 7×11 | stroke `#B0B0B0` | stroke `#B0B0B0` |
| `ArrowUpSelected` | `arrow_up.svg` | 261 | `arrow_up_blue.svg` | 259 | 6×5 | `#E63462` | `#23B6EB` |
| `ArrowUp` | `arrow_up_white.svg` | 261 | `arrow_up_black.svg` | 261 | 6×5 | **`#B1B1B1`** (despite "white") | `#4E4E4E` |
| `ArrowDownSelected` | `arrow_down.svg` | 259 | `arrow_down_blue.svg` | 259 | 6×5 | `#E63462` | `#23B6EB` |
| `ArrowDown` | `arrow_down_white.svg` | 259 | `arrow_down_black.svg` | 259 | 6×5 | **`#B1B1B1`** | `#4E4E4E` |
| `DropDownArrow` | `dropdown_arrow_bw.svg` | 750 | **`dropdown_arrow_bw.svg`** | 750 | 11×7 | stroke `#B0B0B0` | stroke `#B0B0B0` |
| `DropDownArrowHover` | `dropdown_arrow_hover.svg` | 685 | `dropdown_arrow_hover_blue.svg` | 685 | 11×7 | `#E63462` | `#23B6EB` |
| `SliderThumb` | `Slider_Thumb.svg` | 4894 | `Slider_Thumb_blue.svg` | 4894 | 64×64 (art at 16×16) | crimson gradient | teal gradient |
| `SliderThumbBW` | `Slider_Thumb_bw.svg` | 4109 | **`Slider_Thumb_bw.svg`** | 4109 | 16×16 | grey gradient | grey gradient |

Four assets are **shared between themes**: `remove.svg`, `arrow_next_bw.svg`,
`arrow_prev_bw.svg`, `dropdown_arrow_bw.svg`, `Slider_Thumb_bw.svg` — i.e. the
"disabled" art is theme-invariant grey.

### 4.3 The four Drawables cached by `FxTheme`

`FxTheme.cpp:99-102` — rebuilt on **every** `init()`, therefore on every theme switch
(`FxTheme.cpp:497`):

| Member | Source image id | Used by |
|---|---|---|
| `drop_down_arrow_` | `DropDownArrowHover` | enabled combo box (`FxTheme.cpp:161`) |
| `drop_down_arrow_grey_` | `DropDownArrow` | disabled combo box (`FxTheme.cpp:163`) |
| `slider_thumb_` | `SliderThumb` | enabled slider thumb, all three styles (`:206`, `:240`, `:315`) |
| `slider_thumb_grey_` | `SliderThumbBW` | disabled slider thumb (`:208`, `:242`, `:317`) |

Note the naming inversion: the **hover** dropdown arrow is the *normal* state and the
**bw** arrow is the *disabled* state. There is no separate hover art for the combo box.

### 4.4 Vector geometry you can re-draw natively instead of rasterising

Several assets are trivial and should become `egui::Shape` calls, avoiding an SVG
rasteriser entirely:

* `dropdown_arrow_hover.svg` — one polygon, viewBox `0 0 11 7`, points
  `10.2464466,0.646446609  10.9535534,1.35355339  5.8,6.50710678  0.646446609,1.35355339  1.35355339,0.646446609  5.8,5.093`
  (a chevron with ~1 px stroke width baked into the outline).
* `dropdown_arrow_bw.svg` — an open polyline `0,0 → 4.8,4.8 → 9.6,0` stroked at
  `stroke-width: 1` in `#B0B0B0`, translated so it lands inside the 11×7 box.
  In egui: `Shape::line(vec![p0,p1,p2], Stroke::new(1.0, GREY))`.
* `arrow_next.svg` / `arrow_prev.svg` — the *same* chevron polygon rotated by a
  `matrix(0,-1,1,0,-0.096447,11.303554)` / `matrix(0,1,-1,0,7.057106,-0.296447)`,
  viewBox `0 0 7 11`.
* `arrow_up.svg` — `polygon points="3.0,1.25 0.5,3.75 5.5,3.75"` in a `0 0 6 5` box.
* `arrow_down.svg` — `polygon points="0.5,1.25 5.5,1.25 3.0,3.75"`.
* `remove.svg` — `circle cx=8 cy=8 r=7.5` + `line 4,8 → 12,8`, stroke `#D51535` at 1 px.

The **slider thumb** is not trivial and should be rasterised. Its structure
(`Slider_Thumb.svg`):

```
outer circle r=8 at (8,8), viewBox 0 0 64 64 with the art group offset
  fill:   linearGradient 0%→100% diagonal, #D9304F → #DC3253
  stroke: linearGradient 0%→100% diagonal, #D52F4E → #A41A28, 1px, inset at r=7.5
  drop shadow: feOffset dy=4, feGaussianBlur stdDeviation=12,
               colour rgba(0.807843,0.168627,0.278431, 0.479048)
inner circle r=3 at (8,8)
  fill: #0F0F0F
  inner shadow: blur 0.5, dy 1, colour rgba(0.061317,0.071929,0.080418, 1.0)
```

`Slider_Thumb_blue.svg`: outer gradient `#0A4D66 → #0D5F7E`, stroke gradient
`#0A4D66 → #063545`, inner circle `#f0f0f0`.
`Slider_Thumb_bw.svg`: outer gradient `#818181 → #9F9F9F`, stroke gradient
`#9D9D9D → #7B7B7B`, inner circle `#0F0F0F`, inner-shadow colour
`rgba(0.714825,0.714825,0.714825, 0.6)`; viewBox is `0 0 16 16` (the other two are
`0 0 64 64` with a 16×16 art group — **the 64×64 canvas is 4× larger than the ink**,
which matters because JUCE `drawWithin(..., RectanglePlacement::centred, ...)` scales
the *whole viewBox*, not the ink. Reproduce by rasterising the full 64×64 box and
letting the ink occupy the middle quarter, or the thumb will come out 4× too big).

### 4.5 SVG in the Rust port

egui/epaint has **no SVG support**. Two workable routes:

1. **Rasterise at load with `resvg` 0.45 + `usvg` + `tiny-skia`** into an
   `egui::ColorImage`, upload with `ctx.load_texture(...)`, draw with
   `egui::Image::new(&texture)`. Cache key = `(FxImage, FxThemeMode, ceil(logical_size * pixels_per_point))`.
   Re-rasterise on DPI change and on theme change — this mirrors `FxTheme::init()`
   rebuilding the drawables.
2. **Hand-port the trivial ones to `egui::Shape`** (see §4.4) and rasterise only the
   two thumb variants + the logos. This is what I recommend: it keeps the arrows crisp
   at any fractional scale, which Wayland's per-output fractional scaling makes routine.

Wayland fractional scaling caveat: `wp_fractional_scale_v1` gives you e.g. 1.25 or
1.75; rasterising at `round(size * scale)` and letting egui scale the texture produces
visible mush on 6×5 arrows. Use route 2 for anything under ~24 px.

---

## 5. LookAndFeel overrides — exact custom drawing

### 5.1 Geometry constants

`fxsound/Source/GUI/FxTheme.h:42-45`:

| Constant | Value |
|---|---|
| `WINDOW_CORNER_RADIUS` | **21** |
| `TITLE_BAR_HEIGHT` | **57** |
| `SLIDER_THUMB_RADIUS` | **8** |
| `ROTARY_SLIDER_THUMB_RADIUS` | **5** |

Related, from `FxWindow.h:44-45`: `SHADOW_WIDTH = 5`, `CLOSE_BUTTON_WIDTH = 15`.
From `FxWindow.h:71-72`: title-bar logo `ICON_WIDTH = 106`, `ICON_HEIGHT = 15`.

Window frame layout (`FxWindow.cpp:74-82`, `:145-152`):

```
+= shadow_width_ = 5 =========================================+
|  (drop shadow, radius 5, drawn for a rounded rect r=21)     |
|  +---------------------------------------------------+     |
|  |  title bar: x = 21 + 5, y = 5,                     |     |
|  |             w = W - 21*2 - 5*2, h = 57 - 1 = 56    |     |
|  |  [logo 106x15 @ y=(57-15)/2=21]      [x] 15x15     |     |
|  +---------------------------------------------------+     |
|  ----- 1 px rule, colour ControlBackground, y = 56 ---      |
|  |  content: x = 5, y = 57,                           |     |
|  |           size = content's own w x h               |     |
|  +---------------------------------------------------+     |
+=============================================================+
overall size = (content.w + 10, content.h + 56 + 21 + 10)
```
(`FxWindow.cpp:81`: `content_->getHeight() + title_bar_.getHeight() + WINDOW_CORNER_RADIUS + shadow_width_*2`.)

**Linux/Wayland:** there is no server-side title bar to override. Build the whole
frame client-side: `eframe::NativeOptions { viewport: ViewportBuilder::default()
.with_decorations(false).with_transparent(true).with_inner_size(...) }`, paint the
rounded rect + shadow yourself in the root panel, and start drags with
`ctx.send_viewport_cmd(ViewportCommand::StartDrag)` on the title-bar `Response`
(→ `xdg_toplevel.move`). Note: on GNOME/Mutter a transparent undecorated toplevel
gets **no compositor shadow**, so the 5 px client shadow is load-bearing; on KDE it
will double up with the compositor's own. Consider honouring
`org.kde.KWin.Decoration` / `xdg-decoration` and falling back to SSD when the
compositor offers it.

### 5.2 ComboBox

`createComboBoxTextBox` (`FxTheme.cpp:112-118`) — delegates to the base, then sets
`MouseCursor::PointingHandCursor` on **both** the label and the box.

`positionComboBoxText` (`FxTheme.cpp:128-133`):
```cpp
label.setMinimumHorizontalScale(1.0);                 // no auto text squashing
LookAndFeel_V4::positionComboBoxText(box, label);
label.setBounds(label.getBounds().withX(5).withRight(box.getWidth() - 37));
```
→ **text inset: 5 px left, 37 px right.**

`drawComboBox` (`FxTheme.cpp:135-164`):

| Step | Exact value |
|---|---|
| corner radius | `(float)height / 5` |
| body fill | `ComboBox::backgroundColourId` (= `ComboBoxBackground` @ 1.0) |
| outline | `focusedOutlineColourId` if `box.hasKeyboardFocus(true)` else `outlineColourId`; `drawRoundedRectangle(bounds.reduced(0.5, 0.5), cornerSize, 1.0f)` |
| arrow margin | `32`, or **`24` when `width <= 150`** |
| arrow rect | `Rectangle<float>(width - margin, 0, 12, height)`, `RectanglePlacement::centred` |
| arrow drawable | `drop_down_arrow_` if enabled, else `drop_down_arrow_grey_` |

`g.setColour(arrowColourId.withAlpha(enabled ? 1.0 : 0.2))` is issued at
`FxTheme.cpp:159` but is **inert** — `Drawable::drawWithin` paints the SVG's own
fills. `ComboBox::arrowColourId` therefore has no visible effect anywhere.

`drawComboBoxTextWhenNothingSelected` (`FxTheme.cpp:166-179`): text colour =
`ComboBox::textColourId` × **0.5 alpha**; text area = label bounds with **`X = 10`**
minus the label border; line count `max(1, area.height / font.height)`.

```
   +-------------------------------------------------+ <- r = h/5
   | 5px |  "General"                  | 12px arrow  |
   |     |<---- right edge = w - 37 -->|             |
   +-------------------------------------------------+
                            arrow box x = w - 32 (or w - 24 if w <= 150)
```

**egui:** `egui::ComboBox` cannot express this (it hard-codes its own arrow and
padding). Write a custom widget:

```rust
let (rect, resp) = ui.allocate_at_least(vec2(w, h), Sense::click());
let r = h / 5.0;
p.rect_filled(rect, CornerRadius::same(r as u8), pal.combo_box_background);
let stroke_c = if resp.has_focus() { a(pal.slider_highlight, 0.2) } else { pal.combo_box_background };
p.rect_stroke(rect.shrink(0.5), CornerRadius::same(r as u8), Stroke::new(1.0, stroke_c), StrokeKind::Middle);
let margin = if w <= 150.0 { 24.0 } else { 32.0 };
draw_chevron(&p, Rect::from_min_size(rect.min + vec2(w - margin, 0.0), vec2(12.0, h)), enabled);
// text: clip to rect.x + 5 .. rect.right() - 37
resp.on_hover_cursor(CursorIcon::PointingHand)
```
Popup list: `egui::Popup` / `egui::containers::popup` anchored to `resp.rect`.

### 5.3 Linear slider — vertical (`Slider::LinearVertical`)

`FxTheme.cpp:185-216`. This is the EQ band gain slider (`FxEqualizer.cpp:47`, `:97`).

| Element | Exact spec |
|---|---|
| rail | **dashed** line, dash pattern `{5, 2}` (`FxTheme.cpp:187`), thickness `1.0` |
| rail path | `Line(x + width/2, y) → (x + width/2, y + height)` |
| rail paint | vertical `ColourGradient` from `SliderTrack @ 0.4` at `(0, 0)` to `VerticalSliderLow @ 0.4` at `(0, height)` |
| disabled | both gradient stops `withSaturation(0.0)` |
| thumb | radius **8**; rect `(x + width/2 - 8, sliderPos - 8, 16, 16)`, `RectanglePlacement::centred` |
| thumb art | `slider_thumb_` / `slider_thumb_grey_` |
| focus/drag halo | condition `getThumbBeingDragged() >= 0 \|\| hasKeyboardFocus(true)`; colour `SliderHighlight @ 0.1`; rect `(x + (width - 32)/2, y, 32, height)` **expanded by `(0, 8)`**, corner radius **20** |

Note the gradient's absolute coordinates are `(0,0)`–`(0,height)`, **not**
`(x,y)`–`(x,y+height)` — a latent off-by-`y` if the slider is not at the top of its
parent. Reproduce or fix deliberately.

`getSliderLayout` (`FxTheme.cpp:332-351`) trims the track so the 16 px thumb never
clips:
* vertical: `y += SLIDER_THUMB_RADIUS*2` (= +16), `height -= 16`
* horizontal: `width -= SLIDER_THUMB_RADIUS*4` (= −32)

```
   vertical slider, width w, height h
        |
      · |   <- dash 5 on, 2 off, 1px wide, gradient top SliderTrack@.4
      · |
     (O)    <- 16x16 thumb centred on sliderPos
      · |
      · |   <- gradient bottom VerticalSliderLow@.4
        |
   halo when focused/dragged: 32 wide, h+16 tall, r=20, SliderHighlight@.1
```

### 5.4 Linear slider — horizontal (`Slider::LinearHorizontal`)

`FxTheme.cpp:217-250`. Used by the 5 effect sliders, Master Gain, Volume Leveling,
Filter Q (`FxAudioControls.cpp:112`, `:311`, `:329`, `:347`).

| Element | Exact spec |
|---|---|
| rail (unfilled) | `fillRoundedRectangle(x, y + (height - 3)/2, width, 3, 5.6f)`, colour `SliderTrack @ 0.2` |
| rail (filled) | `fillRoundedRectangle(x, y + (height - 3)/2, sliderPos, 3, 5.6f)`, colour `SliderTrack @ 1.0` |
| rail thickness | **3 px**, corner radius **5.6** |
| disabled | `withSaturation(0.0)` on both |
| thumb | `(sliderPos - 8, y + height/2 - 8, 16, 16)` |
| focus halo | `hasKeyboardFocus(true)` only (no drag condition here, unlike vertical); `SliderHighlight @ 0.1`; rect `(x, y, width, height)` expanded by **`(4, 4)`** (`SLIDER_THUMB_RADIUS/2` — integer 8/2), corner radius **`height + 8`** |

**Latent bug to decide on:** the filled rail uses `sliderPos` (an absolute x in the
slider's coordinate space) as a *width*. Correct only while `x == 0`. In the port,
use `slider_pos - rect.left()`.

### 5.5 Rotary slider

`FxTheme.cpp:257-318`. Used for the EQ band centre-frequency wheels
(`FxEqualizer.cpp:53`, `:103`), which set
`setRotaryParameters(3.66519f, 8.90118f, true)` — i.e. **start 3.66519 rad = 210°,
end 8.90118 rad = 510°, sweep 300°**, `stopAtEnd = true`. JUCE angles are measured
clockwise from 12 o'clock.

| Element | Exact spec |
|---|---|
| bounds | `Rectangle(x, y, width, height).toFloat().reduced(2)` |
| `radius` | `min(bounds.w, bounds.h) / 2` |
| `lineW` | **5.0** |
| `arcRadius` | `radius - 2.5` |
| background arc | `addCentredArc(cx, cy, arcRadius, arcRadius, 0, start, end, true)`, stroked with `PathStrokeType(5.0, curved, rounded)`, colour `rotarySliderOutlineColourId` = `SliderTrack @ 0.2` |
| value arc | same, `start → toAngle`, colour `rotarySliderFillColourId` = `SliderTrack @ 1.0` |
| `toAngle` | `start + sliderPos * (end - start)` |
| focus | `DropShadow` with colour `SliderHighlight @ 0.1` drawn **for the background arc path** |
| thumb | radius **5**; centre `(cx + arcRadius·cos(toAngle − π/2), cy + arcRadius·sin(toAngle − π/2))`; rect `(cx' − 5, cy' − 5, 10, 10)` |
| disabled | outline and fill `withSaturation(0.0)` |

**egui:** no rotary widget exists. Custom:
```rust
p.add(Shape::Path(PathShape { points: arc_points(c, arc_r, a0, a1, 64),
                              closed: false, fill: Color32::TRANSPARENT,
                              stroke: PathStroke::new(5.0, colour) }));
```
with round caps approximated by adding `p.circle_filled(end_point, 2.5, colour)` at
both ends — epaint's `PathShape` has no cap style, so this is the fix for the
`PathStrokeType::rounded` look.

### 5.6 Popup menu

`drawPopupMenuItem` (`FxTheme.cpp:353-365`):
```cpp
LookAndFeel_V4::drawPopupMenuItem(g, area, is_separator, is_active,
                                  is_highlighted || is_ticked,   // <-- ticked items render as highlighted
                                  is_ticked, has_submenu, text, shortcut_key_text, icon, text_colour);
if (is_ticked) {
    g.setColour(findColour(PopupMenu::textColourId).withAlpha(1.0f));
    g.drawRect(area.toFloat());                                   // 1px outline, full item rect
}
```
So a checked item gets **both** the highlight fill (`ImageButton` =
`#E63462`/`#23B6EB`) **and** a 1 px outline in `PopupMenu::textColourId`.
This is how the Theme ▸ Dark / Light radio state is shown
(`FxMainWindow.cpp:515-516`, `FxSystemTrayView.cpp:289-290`).

`getPopupMenuFont` → `font_600_ @ 17.0` (`FxTheme.cpp:367-370`).

`preparePopupMenuWindow` (`FxTheme.cpp:372-380`) sets
`MouseCursor::PointingHandCursor` on the popup window **and each direct child**.

**egui:** `egui::menu` / `egui::Popup`. The ticked-item treatment is
`ui.painter().rect_stroke(item_rect, CornerRadius::ZERO, Stroke::new(1.0, pal.menu_text), StrokeKind::Inside)`
after the selectable label. Popup background = `PopupMenu::backgroundColourId`
= `DefaultFill @ 1.0` — note this is **`#000000` in dark and `#FFFFFF` in light**,
i.e. the menu is *not* `MenuBackground` (`#383838`/`#C7C7C7`); that id is only used
by the settings side-nav.

### 5.7 Tooltip

`layoutTooltipText` (`FxTheme.cpp:676-689`):

| Property | Value |
|---|---|
| font | `getNormalFont().withHeight(14.0f)` → Semibold 14 px |
| max width | **400** px |
| word wrap | `AttributedString::WordWrap::byWord` |
| justification | `Justification::centredLeft` |

`getTooltipBounds` (`FxTheme.cpp:515-526`):

| Property | Value |
|---|---|
| width | `textLayout.getWidth() + 20` |
| height | `textLayout.getHeight() + 12` |
| x | `screenPos.x > parentArea.getCentreX() ? screenPos.x - (w + 18) : screenPos.x + 36` |
| y | `screenPos.y > parentArea.getCentreY() ? screenPos.y - (h + 12) : screenPos.y + 12` |
| clamp | `.constrainedWithin(parentArea)` |

`drawTooltip` (`FxTheme.cpp:528-541`):

| Property | Value |
|---|---|
| corner radius | **5.0** |
| fill | `TooltipWindow::backgroundColourId` (JUCE-derived, not overridden here) |
| outline | `TooltipWindow::outlineColourId`, `reduced(0.5, 0.5)`, 1 px |
| text inset | `bounds.reduced(10, 0)` |
| text colour | `TooltipWindow::textColourId`; re-set to `defaultText` per instance at `FxProView.cpp:39` |

Tooltips are suppressed globally by a user setting: `hide_help_tooltips`
(`FxController.cpp:2302-2312`).

**egui:** `Style::spacing.tooltip_width` covers the 400 px cap;
`Visuals::menu_corner_radius = 5`; the ±36/−18 px cursor offsets and the
"flip when past the parent centre" rule are **not** expressible — implement with
`egui::Area::new(id).fixed_pos(computed)` rather than `Response::on_hover_text`.

### 5.8 Scrollbar

`FxTheme` does **not** override `drawScrollbar` / `getDefaultScrollbarWidth`. It only
sets `ScrollBar::thumbColourId = SliderTrack @ 1.0` (`FxTheme.cpp:93`). Thickness is
set per-widget: `setScrollBarThickness(10)` (`FxPresetImportDialog.cpp:59`, `:81`).

So: JUCE's stock `LookAndFeel_V4::drawScrollbar` — a rounded-rect thumb inset by
1 px, no visible track, thumb brightened on hover/drag.

**egui:** `Style::spacing.scroll = ScrollStyle { bar_width: 10.0, floating: false,
bar_inner_margin: 1.0, .. }`, and set
`visuals.widgets.inactive.bg_fill = slider_track`,
`visuals.widgets.hovered.bg_fill = slider_track.lighten()`. No custom painter needed.

### 5.9 Text buttons

`getTextButtonFont` (`FxTheme.cpp:461-464`): `font_600_` at `min(17.0, button_height)`.
Fill = `TextButtonBackground` for both on and off states (`FxTheme.cpp:83-84`), text =
`HighlightedText` for both (`:85-86`). `drawButtonBackground` is **not** overridden, so
the JUCE default rounded-rect button shape applies.

### 5.10 DocumentWindow title bar and buttons — dead code

`drawDocumentWindowTitleBar` (`FxTheme.cpp:543-590`) and
`createDocumentWindowButton` (`FxTheme.cpp:635-674`, plus the private
`FxTheme_DocumentWindowButton` class at `:592-633`) are fully implemented **but
nothing in the app derives from `DocumentWindow` or `ResizableWindow`** — every window
class is `public Component` (grepped across `fxsound/Source/GUI/*.h`; `FxWindow.h:27`
is `class FxWindow : public Component`). The real title bar is
`FxWindow::TitleBar` (`FxWindow.h:60-94`, painted at `FxWindow.cpp:236-259`).

Recorded for completeness, since a future JUCE dialog would pick them up:

* Title bar: fills `widgetBackground`; `Font(12.0f, Font::plain)`; optional icon scaled
  to the font height + 4 px gutter; text `DocumentWindow::textColourId` else
  `defaultText`; `Justification::centredLeft` (`FxTheme.cpp:552-589`).
* Buttons (`FxTheme.cpp:635-674`), all coloured `DefaultFill @ 1.0`, `crossThickness = 0.15f`:
  * close — two line segments `(0,0)→(1,1)` and `(1,0)→(0,1)`
  * minimise — one segment `(0,0.5)→(1,0.5)`
  * maximise — a plus; toggled state is a "restore" glyph built from the polyline
    `45,100 → 0,100 → 0,0 → 100,0 → 100,45` plus `addRectangle(45,45,100,100)`,
    stroked with `PathStrokeType(30.0f)`
* Button paint (`FxTheme.cpp:600-626`): background = `widgetBackground`; colour is
  `colour.withAlpha(0.6f)` when disabled or pressed; when highlighted it fills the
  whole button with the accent and swaps the glyph to the background colour
  (inverse video); glyph fitted into a **centred 12×12** rect.

The **actual** close button is `FxWindow::CloseButton` (`FxWindow.cpp:154-169`):
15×15, background `windowBackground`, an X of two `0.08`-thick segments in
`ImageButton @ 1.0`, fitted to a centred `height × height` square.

### 5.11 Cursor policy

`MouseCursor::PointingHandCursor` is applied to: combo-box label and box
(`FxTheme.cpp:115-116`), popup-menu window and children (`:374-378`), the language
prev/next buttons (`FxLanguage.cpp:33`, `:38`), the window close button
(`FxWindow.cpp:187`), and the export button (`FxPresetExportDialog.cpp:91`).

**egui:** `response.on_hover_cursor(egui::CursorIcon::PointingHand)`. Works on Wayland
via `wp_cursor_shape_v1` (winit ≥ 0.30 uses it when available; otherwise winit falls
back to a themed cursor from `XCURSOR_THEME`).

---

## 6. Localisation

### 6.1 Supported languages — the authoritative list

`FxLanguage.cpp:25` — the cycle order of the settings-dialog language switcher.
**30 entries**, index 0 … 29:

| idx | code | `getLanguageName()` (native) | resource symbol | `language:` header | `countries:` header | strings | bytes |
|---|---|---|---|---|---|---|---|
| 0 | `en` | `English` | `FxSound_txt` | *(template placeholder)* | *(template placeholder)* | 141 | 11274 |
| 1 | `ar` | `العربية` | `FxSound_ar_txt` | Arabic | `EG sa` | 142 | 15263 |
| 2 | `ba` | `bosanski` | `FxSound_ba_txt` | Bosnian | `ba` | 141 | 11676 |
| 3 | `hr` | `hrvatski` | `FxSound_hr_txt` | Croatian | `HR` | 140 | 11525 |
| 4 | `cs` | `Česky` | `FxSound_cs_txt` | Česky | `cz` | 141 | 12136 |
| 5 | `de` | `Deutsch` | `FxSound_de_txt` | German | `de at ch` | 140 | 12889 |
| 6 | `es` | `Español` | `FxSound_es_txt` | Spanish | `ar co es mx` | 141 | 12391 |
| 7 | `fi` | `Suomi` | `FxSound_fi_txt` | Finnish | `fi` | 141 | 12140 |
| 8 | `fr` | `français` | `FxSound_fr_txt` | French | `fr` | 142 | 12787 |
| 9 | `hu` | `Magyar` | `fxsound_hu_txt` **(lower-case `f`)** | Hungarian | `hu` | 140 | 13100 |
| 10 | `id` | `bahasa Indonesia` | `FxSound_id_txt` | Indonesian | `id` | 142 | 11988 |
| 11 | `it` | `Italiano` | `FxSound_it_txt` | Italiano | `it` | 142 | 12281 |
| 12 | `ja` | `日本語` | `FxSound_ja_txt` | Japanese | `ja` | 141 | 13739 |
| 13 | `ko` | `한국어` | `FxSound_ko_txt` | Korean | `kr` | 140 | 12657 |
| 14 | `nl` | `Nederlands` | `FxSound_nl_txt` | *(header missing)* | *(missing)* | 142 | 11934 |
| 15 | `no` | `Norsk` | `FxSound_no_txt` | Norsk | `no` | 136 | 12001 |
| 16 | `fa` | `فارسی` | `FxSound_fa_txt` | Persian | `ir` | 139 | 15942 |
| 17 | `pl` | `Polski` | `FxSound_pl_txt` | Polish | `pl` | 142 | 12699 |
| 18 | `pt` | `Português` | `FxSound_pt_txt` | Portuguese | `pt` | 141 | 12329 |
| 19 | `pt-br` | `português brasileiro` | `FxSound_ptbr_txt` | Brazilian Portuguese | `br` | 142 | 12437 |
| 20 | `ro` | `Română` | `FxSound_ro_txt` | Romanian | `ro` | 142 | 12462 |
| 21 | `ru` | `русский` | `FxSound_ru_txt` | Russian | `ru` | 140 | 16615 |
| 22 | `sl` | `Slovenščina` | `FxSound_sl_txt` | Slovenian | `sl` | 138 | 11762 |
| 23 | `sv` | `svenska` | `FxSound_sv_txt` | Swedish | `se` | 142 | 12066 |
| 24 | `th` | `แบบไทย` | `FxSound_th_txt` | Thai | `th` | 141 | 20011 |
| 25 | `tr` | `Türk` | `FxSound_tr_txt` | Turkish | `tr` | 142 | 12182 |
| 26 | `ua` | `українська` | `FxSound_ua_txt` | Ukrainian | `ua` | 135 | 16213 |
| 27 | `vi` | `Tiếng Việt` | `FxSound_vi_txt` | Vietnamese | `vn` | 142 | 11545 |
| 28 | `zh-CN` | `简体中文` | `FxSound_zhCN_txt` | Chinese (Simplified) | `cn sg` | 140 | 10664 |
| 29 | `zh-TW` | `繁體中文` | `FxSound_zhTW_txt` | Chinese (Traditional) | `Taiwan` | 140 | 10825 |

Sources: codes `FxLanguage.cpp:25`; native names `FxController.cpp:2471-2594`
(stored as `L"\uXXXX"` escapes — e.g. Korean is `L"한국어"` at
`FxController.cpp:2479`); resource symbols and byte sizes
`fxsound/JuceLibraryCode/BinaryData.h:221-309`; mapping code → symbol
`FxController.cpp:2342-2457`; original file paths `fxsound/FxSound.jucer:236-268`
(`../Resources/Strings/FxSound.<code>.txt` — **note `Resources/Strings/` is empty in
this checkout**; the only copies are the byte arrays inside
`fxsound/JuceLibraryCode/BinaryData.cpp`); headers and string counts decoded from
those byte arrays.

Non-standard codes to normalise in the port:

* **`ua` should be `uk`** (ISO 639-1 for Ukrainian; `ua` is the ISO 3166 *country*).
* **`ba` should be `bs`** (ISO 639-1 for Bosnian; `ba` is the country code).
* **`no` / `nb` / `nn`** — the app uses macro-language `no`.
* `pt-br` is lower-case; BCP-47 canonical is `pt-BR`.

### 6.2 Language selection at startup

`FxController::initConfig` (`FxController.cpp:269-278`):
```cpp
if (language.isEmpty()) {                       // no --language= CLI flag
    language = settings_.getString("language");  // persisted choice
    if (language.isEmpty())
        language = SystemStats::getDisplayLanguage();   // OS UI language
}
setLanguage(language);
```
A later `--language=` on a second instance re-applies at runtime
(`FxController.cpp:518-521` via `applyConfig`).

`FxController::setLanguage` (`FxController.cpp:2330-2469`):
1. empty → `"en"` (`:2332-2335`)
2. store in `language_` and persist under key `"language"` (`:2337-2338`)
3. `LocalisedStrings::setCurrentMappings(nullptr)` — clears, so **English is the
   implicit fallback**; `FxSound_txt` (the `en` file) is never actually installed
   (`:2340`)
4. long `startsWithIgnoreCase` chain installing the matching `LocalisedStrings`
   (`:2342-2457`). **Order matters:** `pt-br` (`:2354`) is tested *before* `pt`
   (`:2358`); `zh-CN` (`:2366`) before `zh-TW` (`:2370`); `ko` and `vi` come first.
   The `LocalisedStrings` ctor's second arg is `false` = **case-sensitive lookup**.
5. `theme->loadFont(language_)` (`:2459-2463`)
6. `main_window_->sendLookAndFeelChange()` (`:2465-2468`) — full re-layout

`FxLanguage`'s own index discovery (`FxLanguage.cpp:61-71`) uses
`language_code.startsWith(lng)` with **no early break**, so the *last* match wins:
`"pt-br"` matches `"pt"` at 18 and `"pt-br"` at 19 → index 19. Correct by accident.
But `"en-US"` matches only `"en"` → 0. And an unrecognised code leaves
`language_index_ = -1`, so the first "Next" press lands on index 0 (`en`) via
`++(-1) == 0` (`FxLanguage.cpp:82`), while the first "Prev" press wraps to index 29
via `--(-1) < 0` (`:99-102`).

### 6.3 The string catalogue format

JUCE `LocalisedStrings` text format. File head:
```
language: Arabic
countries: EG sa

"<english source>" = "<translation>"
```
* Entries are one per line, `"src" = "dst"`, with C-style escapes: `\"`, `\'`,
  `\r\n` (literal CRLF in the UI text), `\\`.
* Lookup key is the **English source string itself**, matched case-sensitively.
* Printf placeholders appear in the keys — `%s` in 5 strings, e.g.
  `"Changes to preset %s are saved."`, `"New preset %s is saved."`,
  `"Preset %s is deleted."`, `"FxSound is %s."`,
  `"Preset file %s already exists in the export path, do you want to overwrite the preset file?"`,
  filled via `String::formatted`.
* The English base (`FxSound.txt`, `BinaryData::FxSound_txt`, 11274 bytes) still has
  the Projucer template placeholders in its header
  (`language: [enter full name of the language here!]`) and maps every key to itself.
* `FxSound.nl.txt` is missing its `language:` / `countries:` header entirely — JUCE
  will still parse the mappings but `getLanguageName()`/`getCountryCodes()` return empty.
* String counts per file range **135 (`ua`) … 142**; the English base has **141**.
  So several locales are partially translated and several carry keys the base no
  longer has. Treat mismatches as expected, not as corruption.

### 6.4 The 141 English string keys

The complete key set of `BinaryData::FxSound_txt`, in file order. `\r\n` and `\'`
are shown as they appear in the file.

1. `Oops! There\'s an issue with your playback device settings.\r\nBefore we can get started, please go through the `
2. `troubleshooting steps here.`
3. ` if you\'re still having problems.`
4. `Contact us`
5. `Error in system audio configuration. Unable to run FxSound`
6. `OK`
7. `Click here to see what\'s new on this version!`
8. `FxSound in system tray\r\nClick FxSound icon to reopen`
9. `Thanks for using FxSound! Would you be\r\ninterested in helping us by taking a quick 4 minute\r\nsurvey so we can make FxSound better?`
10. `Take the survey.`
11. `Changes to your preset are not saved.\r\nDo you want to exit?`
12. `Changes to your preset are not saved.\r\nDo you want to ignore the changes?`
13. `Output Disconnected`
14. `Output: `
15. `Changes to preset %s are saved.`
16. `New preset %s is saved.`
17. `Reached the limit on new presets.`
18. `Preset %s is deleted.`
19. `Presets are restored to factory defaults`
20. `Preset file %s already exists in the export path, do you want to overwrite the preset file?`
21. `FxSound is %s.`
22. `on`
23. `off`
24. `Preset: `
25. `Clarity`
26. `Ambience`
27. `Surround Sound`
28. `Dynamic Boost`
29. `Bass Boost`
30. `Enhances and elevates high end\r\nfidelity and presence`
31. `Thickens and smooths audio\r\nwith controlled reverberation`
32. `Widens the left-right balance\r\nfor expansive, wide sound`
33. `Increases overall volume and balance\r\nwith responsive processing`
34. `Boosts low end for full,\r\nimpactful response`
35. `Hyper-low Bass - First band for very low frequencies down to 20 Hz.`
36. `Super-low Bass. Increase this for more rumble and \"thump\", decrease if there\'s too much boominess.`
37. `Center of your Bass sound. Increase this for a fuller low end, decrease if the bass sounds overwhelming.`
38. `The low end of your mid-range. …` *(EQ band 4 help)*
39. `A focal point of the low-mid-range. …` *(band 5)*
40. `The center mid-range band. …` *(band 6)*
41. `The high-mid-range. …` *(band 7)*
42. `The lower end of the high-end range. …` *(band 8)*
43. `The core high-end range. …` *(band 9)*
44. `The highest range of average human hearing. …` *(band 10)*
45. `This wheel allows you to adjust which frequencies this EQ band is affecting…` *(frequency wheel help)*
46. `SUBSCRIBE NOW`
47. `Yes`
48. `No`
49. `Export Presets`
50. `Export`
51. `Select the presets to export...`
52. `Presets are exported successfully!`
53. `Presets successfully imported`
54. `Duplicate presets not imported`
55. `Import Presets`
56. `Import`
57. `Select the folder which contains the presets...`
58. `Folder:`
59. `Preset files not found in the selected folder.`
60. `Settings`
61. `Donate`
62. `General`
63. `Help`
64. `General Preferences`
65. `Launch on system startup`
66. `Automatically switch to newly connected output device`
67. `Hide help tips for audio controls`
68. `Disable keyboard shortcuts`
69. `Reset presets to factory defaults`
70. `Turn FxSound On/Off`
71. `Open/Close FxSound`
72. `Use Next Preset`
73. `Use Previous Preset`
74. `Change Playback Device`
75. `Language`
76. `Disable debug logging`
77. `Version`
78. `Support`
79. `Maintenance`
80. `Changelog`
81. `Quick tour`
82. `Submit debug logs`
83. `Help center`
84. `Feedback`
85. `Check for updates`
86. `Open`
87. `Exit`
88. `Turn Off`
89. `Turn On`
90. `Preset Select`
91. `Playback Device Select`
92. `Enter your preset name`
93. `Enter new preset name`
94. `Overwrite Existing Preset`
95. `Save New Preset`
96. `Undo Preset Changes`
97. `Rename Preset`
98. `Delete Preset`
99. `Download Bonus Presets`
100. `FxSound is unable to play processed audio through the selected output device.\r\nAnother application could be using it in exclusive mode or the device could be\r\ndisconnected. To disable exclusive mode follow these `
101. `steps.`
102. `Click here to save new presets, overwrite old ones, or reset your settings.`
103. `Settings file not found!`
104. `FxSound is now open-source`
105. `Press Ctrl + Alt/Shift + 0-9/A-Z to change the hotkey`
106. `FxSound does not support mono devices, so FxSound processing had been disabled for this device.`
107. `Minimize Button`
108. `Output device`
109. `Select preferred output`
110. `Preferred output:`
111. `None`
112. `Newly connected output device`
113. `Hide notifications`
114. `Audio`
115. `Normalize Volume`
116. `Changes to your preset are not saved.\r\nDo you want to save?`
117. `Save Preset`
118. `Save`
119. `Cancel`
120. `Equalizer:`
121. `Master Gain`
122. `Normalization`
123. `Filter Q`
124. `Balance`
125. `Left`
126. `Right`
127. ` Bands`  *(leading space is significant)*
128. `Restore Defaults`
129. `Automatic updates`
130. `Always On Top`
131. `Theme`
132. `Dark`
133. `Light`
134. `Output Device Preference`
135. `Select preset`
136. `Equalizer`
137. `Prioritize new output devices`
138. `Use Shift+Up or Shift+Down to change the device priority`
139. `Audio processing is not available over Remote Desktop`
140. `Volume Leveling`
141. `Not configured`

Entries 38-45 and 100 are long multi-sentence help texts; they are reproduced in the
source file verbatim and are the ones that exercise the 400 px tooltip wrap
(`FxTheme.cpp:679`).

### 6.5 The `TRANS()` macro and re-translation on language change

`TRANS(x)` is JUCE's `translate()`. Because it resolves at **call time**, most
components call it inside `paint()` so language changes take effect without
reconstruction. Two explicit patterns:

* Toolbar text buttons store the **English** string in `Component::getName()` and
  re-derive the label every paint (`FxWindow.cpp:217-224`, `:250-257`) — the comment
  at `FxWindow.cpp:214-216` explains exactly this.
* The title label re-translates from `name_` on every paint (`FxWindow.cpp:241`).

### 6.6 The language switcher widget (`FxLanguage`)

`FxLanguage.h:29-38`, `FxLanguage.cpp:40-48`:

| Constant | Value |
|---|---|
| `WIDTH` | **180** |
| `HEIGHT` | **30** |
| `BUTTON_WIDTH` | **14** |
| `BUTTON_HEIGHT` | **22** |
| `LABEL_HEIGHT` | **22** |

```
 <-------------------- 180 -------------------->
 +---------------------------------------------+  h = 30, r = 5,
 | 10 | [<] |        Русский            | [>] | 10 |   fill = ControlBackground
 +---------------------------------------------+
       ^14x22 @ y=(30-22)/2=4          ^14x22 @ x=180-14-10=156
       label spans x=24 .. x=156, h=22, y=4, centred
```
(`FxLanguage.cpp:42-44`; background `FxLanguage.cpp:76-77`:
`fillRoundedRectangle(getLocalBounds(), 5.0f)` in `ControlBackground @ 1.0`.)

* prev button: `x = 10` (`:42`); next button: `x = WIDTH - BUTTON_WIDTH - 10 = 156` (`:43`)
* label: `x = prev.getRight() = 24`, `width = next.getX() - 24 = 132`,
  `Justification::centred` (`:28`, `:44`)
* label text colour: `TextButton::textColourOnId` = `HighlightedText` (`:27`)
* arrow images: `ArrowNext`/`ArrowNextBW` and `ArrowPrev`/`ArrowPrevBW` as
  normal/**disabled** (`setImages(normal, nullptr, disabled)` — `:32`, `:37`);
  the middle (over) image is `nullptr`, so there is **no hover art**.
* On click: wrap-around cycle, `FxController::setLanguage(code)`, re-fetch
  `theme.getNormalFont()` (the typeface may have just changed) and set the label text
  to the native name (`:80-95`, `:97-111`).

Placement in the settings dialog: `X_MARGIN`, `LANGUAGE_SWITCH_Y = 50`
(`FxSettingsDialog.h:137`, `FxSettingsDialog.cpp:425`).

**egui:** a 180×30 `Frame` with `CornerRadius::same(5)` and
`fill = pal.control_background`, an `ImageButton`/custom chevron on each side, a
centred `Label`. `ctx.set_fonts()` on change (see §3.6). Persist the code, not the
index — the index is meaningless across versions.

### 6.7 RTL — there is none

Grepped the whole of `fxsound/Source/`: **zero** occurrences of `rtl`,
`rightToLeft`, bidi handling, or mirrored layout. JUCE 6.1.6's `GlyphArrangement`
does no BiDi reordering and no Arabic contextual shaping. So:

* Arabic (`ar`) and Persian (`fa`) ship translated strings and a dedicated font
  request (`IBMPlexSansArabic-*`, `FxTheme.cpp:426-437`), but the UI **does not mirror**
  and the text is laid out left-to-right in isolated glyph forms.
* `FxSound.ar.txt` is 15263 bytes / 142 strings and `FxSound.fa.txt` 15942 / 139 — the
  translations are real; only the rendering is wrong.

**Do not replicate this.** For the Rust port:

| Problem | Fix |
|---|---|
| Arabic/Persian shaping (joining forms, ligatures) | epaint's `ab_glyph`-based layout does **no** shaping. Route text through **`cosmic-text`** (rustybuzz + swash) or **`harfbuzz_rs`** and feed epaint pre-shaped glyph runs, or use an egui text backend crate that already does this. Without it, `ar`/`fa`/`th` will be visibly broken. |
| BiDi reordering | `unicode-bidi` crate, applied per paragraph before shaping. |
| Layout mirroring | egui has no `Direction::RightToLeft` for whole panels. Implement an app-level `is_rtl: bool` and mirror your own layout code: swap `Align::LEFT`/`RIGHT`, mirror the combo-box arrow to the left (`x = margin` instead of `x = w - margin`), reverse the language prev/next arrows, and mirror slider fill direction. |
| Which locales are RTL | `ar`, `fa`. (Thai `th` is LTR but needs shaping + no-space word wrap — `unicode-linebreak` / ICU dictionary breaking.) |

---

## 7. Windows-specific machinery in this subsystem → Linux equivalents

| Windows mechanism | What it achieves | Linux / Wayland / PipeWire replacement |
|---|---|---|
| `LookAndFeel::setDefaultLookAndFeel(&theme_)` (`Main.cpp:63`) | process-wide styling singleton | `egui::Context::set_style()` + `set_visuals()`; keep a `Theme` resource in your app struct, re-apply on change |
| `Typeface::createSystemTypefaceFor(BinaryData::…)` (`FxTheme.cpp:95-97`) | embed TTFs in the PE resource section | `include_bytes!` + `FontData::from_static` for Gilroy only |
| `File::getCurrentWorkingDirectory()` font loading (`FxTheme.cpp:694`) | load extra script fonts from next to the exe | **Do not** use CWD on Linux. Search, in order: `$XDG_DATA_HOME/fxsound/fonts`, each `$XDG_DATA_DIRS/fxsound/fonts`, `/usr/share/fxsound/fonts`. Better: depend on `noto-fonts-cjk`, `noto-fonts-extra` and resolve via `fontconfig` (`fontconfig` / `font-kit` crate) so you ship 0 MB of CJK |
| `SetCurrentDirectory(exe_dir)` (`Main.cpp:308-321`) | make relative resource paths work | unnecessary; use absolute XDG paths |
| `SystemStats::getDisplayLanguage()` (`FxController.cpp:274`) | OS UI language | `sys-locale::get_locale()`, or read `LC_ALL` → `LC_MESSAGES` → `LANG`, strip `.UTF-8`/`@modifier`, map `_` → `-` |
| `PropertiesFile` in `%APPDATA%\FxSound\` (`Settings.cpp:42-49`, `Settings.h:29-32`) | persist `theme_mode`, `language` | `$XDG_CONFIG_HOME/fxsound/settings.toml` via `directories` + `serde`/`toml`. Keep the same key names (`theme_mode`, `language`) to ease migration |
| `LoadIcon(hInst, L"IDI_LOGO_RED"/"_BLUE"/"_WHITE"/"_GRAY")` (`FxSystemTrayView.cpp:94-100`, `:183-195`; `FxMainWindow.cpp:379-391`; resources at `fxsound/Project/status_icons.rc`) | 4 tray/window icon states: red = dark+processing, blue = light+processing, white = powered but idle, grey = powered off | Install `fxsound-active.svg` / `fxsound-active-light.svg` / `fxsound-idle.svg` / `fxsound-off.svg` into `/usr/share/icons/hicolor/scalable/apps/`. Tray = **StatusNotifierItem** over D-Bus via the `ksni` crate. Window icon = `ViewportBuilder::with_icon(IconData)` (Wayland ignores it for the taskbar — the compositor uses the `.desktop` file's `Icon=` matched by `app_id`, so also call `with_app_id("com.fxsound.FxSound")` and ship a matching `.desktop`) |
| `Shell_NotifyIcon` / `NOTIFYICONDATA` (`FxSystemTrayView.cpp:86-89`) | system tray | `ksni` (SNI/`org.kde.StatusNotifierItem`). **Caveat:** GNOME ships no SNI host by default (needs the AppIndicator extension). Provide a `--no-tray` mode and a normal window as the fallback |
| `RegOpenKeyEx(HKEY_CURRENT_USER, "…\\CurrentVersion\\Run")` (`FxController.cpp:2792`) | launch on startup | write `~/.config/autostart/fxsound.desktop` (XDG Autostart), `X-GNOME-Autostart-enabled=true` |
| `HWND` / `SendMessage(WM_GETICON)` (`FxMainWindow.cpp:370-372`) | per-window icon swap on theme change | no Wayland equivalent; the icon comes from the `.desktop` entry. Drop this; update only the tray icon |
| `ComponentDragger` on the title bar (`FxWindow.cpp:346-352`) | drag a borderless window | `ctx.send_viewport_cmd(ViewportCommand::StartDrag)` → `xdg_toplevel.move`. Must be sent during the pointer-press event; you cannot set an absolute window position on Wayland at all |
| `DropShadow` for the window (`FxWindow.cpp:124-129`) | shadow around the 21 px rounded frame | client-side: `Shadow { blur: 5, .. }` on a transparent surface. Requires `with_transparent(true)` **and** an alpha-capable swapchain (`wgpu` `CompositeAlphaMode::PreMultiplied`) |
| Win32 global hotkeys (`cmd_on_off = 393297`, `cmd_open_close = 393285`, `cmd_next_preset = 393281`, `cmd_previous_preset = 393306`, `cmd_change_output = 393303` — `Settings.cpp:34-38`) | system-wide keyboard shortcuts | **A Wayland client cannot register global hotkeys.** Options, in order of preference: (1) XDG portal `org.freedesktop.portal.GlobalShortcuts` (xdg-desktop-portal ≥ 1.17; KDE has it, GNOME does not yet); (2) MPRIS2 `org.mpris.MediaPlayer2` for play/pause-adjacent actions; (3) ship a documented set of compositor keybindings invoking the existing CLI (`docs/COMMAND_LINE_OPTIONS.md`) against the single-instance socket — this is the reliable path today |
| `--language=` on a second instance → `anotherInstanceStarted` (`Main.cpp:136-139`) | single-instance IPC | a Unix socket in `$XDG_RUNTIME_DIR/fxsound.sock` (or D-Bus name ownership + `org.freedesktop.Application` `Activate`/`ActivateAction`) |
| audio device names shown in combo boxes | WASAPI enumeration | PipeWire node enumeration (`pipewire-rs` / `libspa`); device labels come from `node.description` / `node.nick` |

---

## 8. Port checklist (theme subsystem)

1. `Palette` struct with 27 `Color32` fields; two `const` instances built from the hex
   values in §2.3. Unit-test them against the table.
2. `fn visuals(p: &Palette, dark: bool) -> egui::Visuals` per §2.6.
3. `fn desaturate(Color32) -> Color32` (HSV S→0, **not** luma) per §2.7.
4. `FontSet` with three `FontFamily::Name` entries; `fn apply_fonts(ctx, lang)` per §3.6.
5. Icon registry: `enum FxImage` (32 variants, order per §4.1) →
   `fn svg(img, mode) -> &'static [u8]` plus a `HashMap<(FxImage, Mode, u32), TextureHandle>`
   cache, cleared on theme change and on `pixels_per_point` change.
6. Custom widgets (unavoidable, egui has no stock equivalent):
   `fx_combo_box`, `fx_slider_h`, `fx_slider_v`, `fx_knob`, `fx_title_bar`,
   `fx_tooltip`, `fx_language_switcher`, `fx_menu_item` (ticked outline).
   Everything else can be stock egui with the `Visuals` from step 2.
7. Localisation: parse the JUCE `.txt` format (30 files, ~142 keys) into
   `HashMap<&'static str, String>`; or convert to `.ftl` at build time and use
   `fluent-bundle`. Keep English source strings as keys so the catalogues port 1:1.
8. Text stack: **do not** ship the default epaint layout for `ar`, `fa`, `th`.

---

## 9. Open questions / risks for the Rust port

1. **JUCE-derived colour ids are not in this tree.** JUCE itself is not vendored
   here (no `juce_LookAndFeel_V4.cpp` anywhere on disk), so the ~130 component colours
   that `setColourScheme()` derives from the 9 slots — including
   `TooltipWindow::backgroundColourId`, `PopupMenu::textColourId`,
   `Label::textColourId`, `ListBox::*`, `ScrollBar::backgroundColourId` — could not be
   read and are **not** stated in this document. Before finalising the port, open
   JUCE 6.1.6 `modules/juce_gui_basics/lookandfeel/juce_LookAndFeel_V4.cpp`
   (`initialiseColours()`) and transcribe them. Tooltip background in particular is
   visible in every screenshot and is currently unspecified here.
2. **JUCE `Font` height vs. egui `FontId::size`.** JUCE's `withHeight(17.0f)` sets the
   *total* ascent+descent box; egui's `FontId::size` is passed to `ab_glyph` as a
   scale in px, and `row_height` comes out as `ascent - descent + line_gap` for that
   scale. Expect 17.0 to render ~5-10 % differently. Calibrate against a screenshot of
   the Windows build before freezing the numbers; if you need exact parity, solve for
   `size` such that `fonts.row_height(&font_id) == 17.0`.
3. **Gilroy is a commercial font.** It is checked into `fxsound/Fonts/` and embedded
   in the Windows binary, but the repo is AGPL-3.0 and Gilroy (Radomir Tinkov) is not.
   Redistributing a Linux package with these TTFs may not be licensable. Plan a
   fallback: Inter, Figtree, or Manrope are the closest geometric-humanist
   substitutes; all are OFL. Budget a layout pass — Gilroy is noticeably narrower than
   Inter at the same px height, and several fixed-width labels (the 132 px language
   label, the `width - 37` combo text area) have no slack.
4. **The 64×64 vs 16×16 slider-thumb viewBox trap** (§4.4). `Slider_Thumb.svg` and
   `Slider_Thumb_blue.svg` declare a 64×64 canvas with the ink only in a 16×16 region
   placed by a chain of nested `transform="translate(...)"` groups; `Slider_Thumb_bw.svg`
   declares 16×16. JUCE's `drawWithin(rect, centred, 1.0)` fits the whole viewBox, so
   in the Windows build the enabled thumb draws at ¼ the size of the disabled one
   inside the same 16×16 rect. **Verify this against a screenshot** — either it is a
   real visual bug in the shipping app, or JUCE's SVG parser is tightening the bounds
   to the ink. The answer changes what "pixel-accurate" means here.
5. **`zh-TW` font extension mismatch** (§3.3 defect 1) — Traditional Chinese is almost
   certainly broken in the shipping Windows build. Decide whether the Linux port
   reproduces the bug (no) or fixes it (yes) and note the deviation.
6. **Four fonts referenced but absent** (`MontserratAlternates-*`, `NotoSansJP-*`,
   `IBMPlexSansArabic-*`). They may be shipped by the Windows installer, which is not
   in this tree. Confirm against a real install before assuming `ja`/`vi`/`ar`/`fa`
   are Gilroy-fallback in production.
7. **`Resources/Strings/` is empty in this checkout.** The catalogues exist only as
   byte arrays inside `fxsound/JuceLibraryCode/BinaryData.cpp` (a Projucer artefact).
   The port must extract them from there (a ~30-line Python decoder over the
   `temp_binary_data_NN` arrays works; some are C string literals, some are
   `{0x..,}` arrays). Do not assume upstream will regenerate the plain files.
8. **Language codes `ua` and `ba` are wrong** (§6.1). If you normalise to `uk`/`bs`,
   write a migration for the persisted `"language"` value or returning users lose
   their choice.
9. **RTL is a genuinely new feature, not a port.** §6.7. Scoping it as "make Arabic
   work" means adopting `cosmic-text` or `harfbuzz_rs` plus `unicode-bidi` plus a
   mirrored-layout pass through every custom widget. If that is out of budget, ship
   `ar`/`fa` disabled rather than visibly broken.
10. **Wayland has no global hotkeys.** Five hotkey actions are core to FxSound's
    product identity (`Settings.cpp:34-38`, strings 70-74). On GNOME today there is no
    portal for this. Decide early: portal-where-available + documented compositor
    bindings, or accept the feature is degraded, and say so in the README.
11. **Rounded corners + client shadow on Wayland** are compositor-dependent. Under
    `xdg-decoration` with SSD forced (KDE's default for some apps), your 21 px corners
    and 57 px title bar will be drawn *inside* a server frame. Handle the
    `zxdg_toplevel_decoration_v1.configure` event and switch to a plain rectangular
    layout when the server takes over decorations.
12. **`ctx.set_fonts()` rebuilds the whole glyph atlas.** With an 8.5 MB Noto SC face
    that is a multi-frame stall on language switch. FxSound does the same thing
    (`FxController.cpp:2462`) and gets away with it because the switch is rare.
    Consider doing the rebuild on a worker thread and swapping in the `FontDefinitions`
    on the next frame, or pre-warming the atlas with the ~142 catalogue strings.
13. **egui 0.36 API names.** This spec uses `CornerRadius` (not `Rounding`),
    `Visuals::window_corner_radius`, `WidgetVisuals::corner_radius`,
    `Shadow { offset: [i8; 2], blur: u8, spread: u8, color }`,
    `Arc<FontData>` in `FontDefinitions::font_data`, and `StrokeKind` on
    `Painter::rect_stroke`. Verify each against the 0.36.0 docs before writing code —
    these names churned across 0.28 → 0.31 → 0.36.
14. **Colour-space of alpha blending.** JUCE composites in straight (non-premultiplied)
    sRGB with no gamma correction. egui/epaint blends in linear space by default
    (`Visuals::numeric_color_space` / the renderer's sRGB handling). The many
    `α = 0.1` / `0.2` / `0.34` overlays in §2.3 will come out **lighter** in egui than
    in the Windows build unless you match the blend space. Test the EQ curve fill
    (`EqStart @ 0.34` over `ControlBackground`) side by side; it is the most visible case.
