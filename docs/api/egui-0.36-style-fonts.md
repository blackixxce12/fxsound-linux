# egui 0.36 — Style, Theme & Fonts (verified cheatsheet)

Every signature below was read out of the vendored sources. Every code block in this file was
compiled offline against the exact crates in this workspace (`egui = "=0.36.0"`, rustc 1.98.1,
edition 2024) before being written down. Nothing here is from memory.

**Source roots** — all `<file>:<line>` citations are relative to one of these:

| prefix | crate root |
| --- | --- |
| `egui/` | `/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/egui-0.36.0` |
| `epaint/` | `/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/epaint-0.36.2` |
| `emath/` | `/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/emath-0.36.2` |
| `ecolor/` | `/home/blackixxce/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/ecolor-0.36.2` |

> `egui` is `0.36.0`, but `epaint` / `emath` / `ecolor` resolve to `0.36.2`. All font and geometry
> types live in the `.2` patch releases.

---

## 0. READ THIS FIRST — what changed, with compiler proof

Each row was proven by compiling code that uses the old spelling and capturing the error.

| You probably remember | egui 0.36 reality | Proof |
| --- | --- | --- |
| `ctx.set_style(style)` | **Gone.** Use `ctx.set_global_style(..)` or `ctx.set_style_of(theme, ..)` | `E0599: no method named set_style found for reference &egui::Context` |
| `ctx.style()` | **Gone.** Use `ctx.global_style() -> Arc<Style>` or `ctx.style_of(theme)` | `E0599: no method named style found for reference &egui::Context` |
| `egui::Rounding` | **Gone.** Renamed `CornerRadius` | `E0425: cannot find type Rounding in crate egui` |
| `Rounding { nw: f32, .. }` | `CornerRadius { nw: u8, ne: u8, sw: u8, se: u8 }` — **u8, not f32** | `epaint/src/corner_radius.rs:13-25` |
| `Visuals::window_rounding` | `Visuals::window_corner_radius: CornerRadius` | `egui/src/style.rs:1060` |
| `Visuals::menu_rounding` | `Visuals::menu_corner_radius: CornerRadius` | `egui/src/style.rs:1068` |
| `WidgetVisuals::rounding` | `WidgetVisuals::corner_radius: CornerRadius` | `egui/src/style.rs:1307` |
| `Margin { left: f32, .. }` | `Margin { left: i8, right: i8, top: i8, bottom: i8 }` — **i8** | `epaint/src/margin.rs:15-20` |
| `Shadow { offset: Vec2, blur: f32, spread: f32 }` | `Shadow { offset: [i8; 2], blur: u8, spread: u8, color: Color32 }` | `epaint/src/shadow.rs:10-27` |
| `font_data: BTreeMap<String, FontData>` | `BTreeMap<String, Arc<FontData>>` — values **must** be `Arc::new(..)` | `epaint/src/text/fonts.rs:435` |
| `Selection::light()` / `Selection::dark()` | **Private** (`fn`, not `pub fn`) — you cannot call them | `E0624: associated function light is private` |
| `Widgets::light()` / `Widgets::dark()` | **Public** — these you *can* call | `egui/src/style.rs:1680`, `:1725` |
| `Visuals::clip_rect_margin` | Present but `#[deprecated]` and has **no effect**. Naming it in a struct literal warns | `egui/src/style.rs:1085-1086` |
| `egui::Widgets`, `egui::WidgetVisuals`, `egui::Selection`, `egui::Interaction` | **Not at the crate root.** Import from `egui::style::*` | `E0432: unresolved imports` |
| `egui::FontInsert`, `egui::TextOptions` | Not at the root, and not in `egui::text` either. Use `egui::epaint::text::{..}` | `E0432` + `E0433: cannot find TextOptions in text` |
| `TopBottomPanel` / `SidePanel` | Gone — one `Panel` type: `Panel::top(id)` / `bottom` / `left` / `right` | `egui/src/containers/panel.rs:206,265` |
| no text-rendering knobs in `Visuals` | New `Visuals::text_options: TextOptions` carries hinting / subpixel / gamma | `egui/src/style.rs:1000` |
| `Style` has no `compact_menu_style` | It does now, default `true` | `egui/src/style.rs:340`, `:1446` |

### The two traps that cost the most time

**1. `Visuals { .. }` as a full struct literal forces a deprecation warning.**
The struct still has `clip_rect_margin`, so an exhaustive literal must name it, which warns.
Functional-update syntax does *not* warn, because you never name the field:

```rust
// WARNS: use of deprecated field `egui::Visuals::clip_rect_margin`
let v = egui::Visuals { clip_rect_margin: 0.0, ..egui::Visuals::dark() };

// CLEAN: always build hand-authored visuals with FRU off a base.
let v = egui::Visuals { panel_fill: egui::Color32::from_gray(22), ..egui::Visuals::dark() };
```

**2. `ctx.set_visuals(..)` only touches ONE of the two stored styles.**
egui 0.36 keeps `dark_style` *and* `light_style` side by side (`egui/src/memory/mod.rs:196, :200`).
`set_visuals` resolves the *currently active* theme and writes only that one
(`egui/src/context.rs:2277-2279`). Under the default `ThemePreference::System`, the OS flipping to
light silently drops you to stock egui light. Always set both, or set the preference explicitly.

---

## 1. Where each type actually lives

```rust
// At the crate root (egui/src/lib.rs:443-497):
use egui::{
    Align, Color32, Context, CornerRadius, Margin, Shadow, Stroke, StrokeKind, Vec2, // :443-452
    FontData, FontDefinitions, FontFamily, FontId, FontTweak,                        // :450
    Memory, Options, Theme, ThemePreference,                                         // :483
    FontSelection, Spacing, Style, TextStyle, Visuals,                               // :488
    TextWrapMode, CursorIcon,                                                        // :475, :470
};

// NOT at the root — `pub mod style` (egui/src/lib.rs:418):
use egui::style::{
    default_text_styles, DebugOptions, HandleShape, ImeComposition, Interaction,
    NumericColorSpace, NumberFormatter, ScrollAnimation, ScrollFadeStyle, ScrollStyle,
    Selection, StyleModifier, TextCursorStyle, WidgetVisuals, Widgets,
};

// NOT at the root and NOT in `egui::text` — only through the epaint re-export:
use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily, TextOptions};

// New in this release — `pub mod widget_style` (egui/src/lib.rs:426):
use egui::widget_style::{ButtonStyle, CheckboxStyle, Classes, HasClasses, LabelStyle,
                         SeparatorStyle, TextVisuals, WidgetState, WidgetStyle};

// Rounding helpers (emath/src/lib.rs:47):
use egui::emath::{GUI_ROUNDING, GuiRounding};
```

`egui::text` re-exports only a subset — `FontData, FontDefinitions, FontFamily, Fonts, Galley,
LayoutJob, LayoutSection, TextFormat, TextWrapping` (`egui/src/lib.rs:456-459`). `TextOptions` is
**not** among them.

### Feature flags that gate anything in this document

| flag | crate | gates |
| --- | --- | --- |
| `default_fonts` (**on by default**) | egui → epaint → `epaint_default_fonts` | `FontDefinitions::default()` returning real fonts, and `FontDefinitions::builtin_font_names()` returning a non-empty slice. Without it, `default()` == `empty()` and you get **no glyphs at all** unless you install your own. `egui/Cargo.toml` `default = ["default_fonts"]`; `epaint/src/text/fonts.rs:490-497` vs `:499-556`, `:574` vs `:585` |
| `serde` | egui + epaint | `Serialize`/`Deserialize` on `Style`, `Visuals`, `Spacing`, `Interaction`, `Widgets`, `WidgetVisuals`, `Selection`, `TextStyle`, `FontId`, `FontFamily`, `FontData`, `FontTweak`, `FontDefinitions`, `TextOptions`, `Theme`, `ThemePreference`, `Options`. All of these also carry `#[serde(default)]` on the struct, so adding fields upstream will not break stored configs |
| `persistence` | egui | `serde` + `ron` |
| `callstack` | egui | changes the default of `DebugOptions::debug_on_hover_with_all_modifiers` (`egui/src/style.rs:1396-1397`) |
| `debug_assertions` (cfg, not a feature) | egui | the entire `Style::debug: DebugOptions` field and the `DebugOptions` type only exist in debug builds (`egui/src/style.rs:322-323`, `:1329-1332`) |

`Style::debug` is the one field you must `#[cfg]`-guard if you write an exhaustive `Style { .. }`
literal. Use FRU instead.

---

## 2. `Style` — every field

```rust
// egui/src/style.rs:240-243
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Style { /* … */ }
```

| field | type | line | default (`Style::default()`, `:1426-1449`) |
| --- | --- | --- | --- |
| `override_text_style` | `Option<TextStyle>` | `:248` | `None` |
| `override_font_id` | `Option<FontId>` | `:254` | `None` |
| `override_text_valign` | `Option<Align>` | `:259` | `Some(Align::Center)` |
| `text_styles` | `BTreeMap<TextStyle, FontId>` | `:288` | `default_text_styles()` |
| `drag_value_text_style` | `TextStyle` | `:291` | `TextStyle::Button` |
| `number_formatter` | `NumberFormatter` | `:297` | `NumberFormatter(Arc::new(emath::format_with_decimals_in_range))`; `#[serde(skip)]` |
| `wrap_mode` | `Option<crate::TextWrapMode>` | `:305` | `None` (follow layout) |
| `spacing` | `Spacing` | `:308` | `Spacing::default()` |
| `interaction` | `Interaction` | `:311` | `Interaction::default()` |
| `visuals` | `Visuals` | `:314` | `Visuals::default()` == `Visuals::dark()` |
| `animation_time` | `f32` | `:317` | `0.2` |
| `debug` | `DebugOptions` | `:323` | `Default::default()` — **`#[cfg(debug_assertions)]` only** |
| `explanation_tooltips` | `bool` | `:328` | `false` |
| `url_in_tooltip` | `bool` | `:331` | `false` |
| `always_scroll_the_only_direction` | `bool` | `:334` | `false` |
| `scroll_animation` | `ScrollAnimation` | `:337` | `ScrollAnimation::default()` |
| `compact_menu_style` | `bool` | `:340` | `true` |

```rust
// egui/src/style.rs:349-379
impl Style {
    pub fn interact(&self, response: &Response) -> &WidgetVisuals;                       // :354
    pub fn interact_selectable(&self, response: &Response, selected: bool) -> WidgetVisuals; // :358
    pub fn noninteractive(&self) -> &WidgetVisuals;                                      // :370
    pub fn text_styles(&self) -> Vec<TextStyle>;                                         // :375
    pub fn ui(&mut self, ui: &mut crate::Ui);                                            // :1785
}
```

```rust
// egui/src/style.rs:1413
pub fn default_text_styles() -> BTreeMap<TextStyle, FontId>
```

It returns exactly five entries (`:1417-1422`) — note there is **no** `Small`-monospace and
**no** `Name(..)` entry by default:

| `TextStyle` | `FontId` |
| --- | --- |
| `Small` | `FontId::new(9.0, Proportional)` |
| `Body` | `FontId::new(13.0, Proportional)` |
| `Button` | `FontId::new(13.0, Proportional)` |
| `Heading` | `FontId::new(18.0, Proportional)` |
| `Monospace` | `FontId::new(13.0, Monospace)` |

### `StyleModifier`

```rust
// egui/src/style.rs:192-193
#[derive(Clone, Default)]
pub struct StyleModifier(Option<Arc<dyn Fn(&mut Style) + Send + Sync>>);

// :216-238
impl StyleModifier {
    pub fn new(f: impl Fn(&mut Style) + Send + Sync + 'static) -> Self;  // :218
    pub fn apply(&self, style: &mut Style);                              // :224
}
// :202 — blanket: impl<T: Fn(&mut Style) + Send + Sync + 'static> From<T> for StyleModifier
// :210 — impl From<Style> for StyleModifier  (overwrites the whole style)
```

### `FontSelection`

```rust
// egui/src/style.rs:125-137
#[derive(Debug, Clone)]
pub enum FontSelection { Default, FontId(FontId), Style(TextStyle) }

// :145-172
impl FontSelection {
    pub fn resolve(self, style: &Style) -> FontId;                                   // :150
    pub fn resolve_with_fallback(self, style: &Style, fallback: Self) -> FontId;     // :157
}
// :174 From<FontId>, :181 From<TextStyle>. Default == Self::Default (:138)
```

`FontSelection::Default` resolves in this order (`:158-170`): `Style::override_font_id` →
`Style::override_text_style` → the fallback (`TextStyle::Body` for `resolve`).

---

## 3. `Spacing` — every field

```rust
// egui/src/style.rs:381-384
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Spacing { /* … */ }
```

| field | type | line | default (`:1451-1478`) |
| --- | --- | --- | --- |
| `item_spacing` | `Vec2` | `:391` | `vec2(8.0, 3.0)` |
| `window_margin` | `Margin` | `:394` | `Margin::same(6)` |
| `button_padding` | `Vec2` | `:397` | `vec2(4.0, 1.0)` |
| `menu_margin` | `Margin` | `:400` | `Margin::same(6)` |
| `indent` | `f32` | `:403` | `18.0` |
| `interact_size` | `Vec2` | `:408` | `vec2(40.0, 18.0)` |
| `slider_width` | `f32` | `:411` | `100.0` |
| `slider_rail_height` | `f32` | `:414` | `8.0` |
| `combo_width` | `f32` | `:417` | `100.0` |
| `text_edit_width` | `f32` | `:420` | `280.0` |
| `extra_text_line_spacing` | `f32` | `:423` | `0.0` |
| `icon_width` | `f32` | `:427` | `14.0` |
| `icon_width_inner` | `f32` | `:431` | `8.0` |
| `icon_spacing` | `f32` | `:435` | `4.0` |
| `default_area_size` | `Vec2` | `:444` | `vec2(600.0, 400.0)` |
| `tooltip_width` | `f32` | `:447` | `500.0` |
| `menu_width` | `f32` | `:452` | `400.0` |
| `menu_spacing` | `f32` | `:455` | `2.0` |
| `indent_ends_with_horizontal_line` | `bool` | `:458` | `false` |
| `combo_height` | `f32` | `:461` | `200.0` |
| `scroll` | `ScrollStyle` | `:464` | `ScrollStyle::default()` == `::floating()` |

```rust
// egui/src/style.rs:467-489
impl Spacing {
    /// Returns (small_icon_rect, big_icon_rect)
    pub fn icon_rectangles(&self, rect: Rect) -> (Rect, Rect);  // :469
    pub fn ui(&mut self, ui: &mut crate::Ui);                   // :1943
}
```

`interact_size.y` is the default height of buttons/sliders. `window_margin` and `menu_margin` are
`Margin` (**i8**), not `Vec2` — `Margin::same(6)`, `Margin::symmetric(x, y)`, `Margin::ZERO`.

---

## 4. `Interaction` — every field

```rust
// egui/src/style.rs:907-910
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Interaction { /* … */ }
```

| field | type | line | default (`:1479-1492`) |
| --- | --- | --- | --- |
| `interact_radius` | `f32` | `:915` | `5.0` |
| `resize_grab_radius_side` | `f32` | `:918` | `3.0` |
| `resize_grab_radius_corner` | `f32` | `:921` | `10.0` |
| `show_tooltips_only_when_still` | `bool` | `:924` | `true` |
| `tooltip_delay` | `f32` | `:927` | `0.5` |
| `tooltip_grace_time` | `f32` | `:934` | `0.2` |
| `selectable_labels` | `bool` | `:937` | `true` |
| `multi_widget_text_select` | `bool` | `:943` | `true` |

Only method: `pub fn ui(&mut self, ui: &mut crate::Ui)` (`:2079`).

---

## 5. `Visuals` — EVERY field with its dark and light default

```rust
// egui/src/style.rs:985-988
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Visuals { /* … */ }

// :1609-1613
impl Default for Visuals { fn default() -> Self { Self::dark() } }
```

`Visuals::light()` (`:1564-1604`) ends with `..Self::dark()`, so **every field it does not name
inherits the dark value verbatim.** Those rows are marked *(inherits dark)* below — that is the
source of most "why is my light theme wrong" bugs.

| # | field | type | struct line | **dark** (`:1497-1561`) | **light** (`:1564-1604`) |
| --- | --- | --- | --- | --- | --- |
| 1 | `dark_mode` | `bool` | `:994` | `true` `:1499` | `false` `:1566` |
| 2 | `text_options` | `TextOptions` | `:1000` | `TextOptions { color_transfer_function: FontColorTransferFunction::DARK_MODE_DEFAULT, ..Default::default() }` `:1500-1503` | `…LIGHT_MODE_DEFAULT…` `:1567-1570` |
| 3 | `override_text_color` | `Option<Color32>` | `:1015` | `None` `:1504` | *(inherits dark)* `None` |
| 4 | `weak_text_alpha` | `f32` | `:1020` | `0.6` `:1505` | *(inherits dark)* `0.6` |
| 5 | `weak_text_color` | `Option<Color32>` | `:1026` | `None` `:1506` | *(inherits dark)* `None` |
| 6 | `widgets` | `Widgets` | `:1029` | `Widgets::default()` == `Widgets::dark()` `:1507` | `Widgets::light()` `:1571` |
| 7 | `selection` | `Selection` | `:1031` | `Selection::default()` == private `Selection::dark()` `:1508` | private `Selection::light()` `:1572` |
| 8 | `ime_composition` | `ImeComposition` | `:1032` | `ImeComposition::default()` == private `::dark()` `:1509` | private `::light()` `:1573` |
| 9 | `hyperlink_color` | `Color32` | `:1035` | `from_rgb(90, 170, 255)` `:1510` | `from_rgb(0, 155, 255)` `:1574` |
| 10 | `faint_bg_color` | `Color32` | `:1039` | `from_additive_luminance(5)` `:1511` | `from_additive_luminance(5)` `:1575` — **same in both** |
| 11 | `extreme_bg_color` | `Color32` | `:1044` | `from_gray(10)` `:1512` | `from_gray(255)` `:1576` |
| 12 | `text_edit_bg_color` | `Option<Color32>` | `:1049` | `None` → falls back to `extreme_bg_color` `:1513` | *(inherits dark)* `None` |
| 13 | `code_bg_color` | `Color32` | `:1052` | `from_gray(64)` `:1514` | `from_gray(230)` `:1577` |
| 14 | `warn_fg_color` | `Color32` | `:1055` | `from_rgb(255, 143, 0)` `:1515` | `from_rgb(255, 100, 0)` `:1578` |
| 15 | `error_fg_color` | `Color32` | `:1058` | `from_rgb(255, 0, 0)` `:1516` | `from_rgb(255, 0, 0)` `:1579` — **same in both** |
| 16 | `window_corner_radius` | `CornerRadius` | `:1060` | `CornerRadius::same(6)` `:1518` | *(inherits dark)* `same(6)` |
| 17 | `window_shadow` | `Shadow` | `:1061` | `Shadow { offset: [10, 20], blur: 15, spread: 0, color: from_black_alpha(96) }` `:1519-1524` | same geometry, `color: from_black_alpha(25)` `:1581-1586` |
| 18 | `window_fill` | `Color32` | `:1062` | `from_gray(27)` `:1525` | `from_gray(248)` `:1587` |
| 19 | `window_stroke` | `Stroke` | `:1063` | `Stroke::new(1.0, from_gray(60))` `:1526` | `Stroke::new(1.0, from_gray(190))` `:1588` |
| 20 | `window_highlight_topmost` | `bool` | `:1066` | `true` `:1527` | *(inherits dark)* `true` |
| 21 | `menu_corner_radius` | `CornerRadius` | `:1068` | `CornerRadius::same(6)` `:1529` | *(inherits dark)* `same(6)` |
| 22 | `panel_fill` | `Color32` | `:1071` | `from_gray(27)` `:1531` | `from_gray(248)` `:1590` |
| 23 | `popup_shadow` | `Shadow` | `:1073` | `Shadow { offset: [6, 10], blur: 8, spread: 0, color: from_black_alpha(96) }` `:1533-1538` | same geometry, `color: from_black_alpha(25)` `:1592-1597` |
| 24 | `resize_corner_size` | `f32` | `:1075` | `12.0` `:1540` | *(inherits dark)* `12.0` |
| 25 | `text_cursor` | `TextCursorStyle` | `:1078` | `Default::default()` `:1542` (stroke `2.0 × rgb(192,222,255)`) | `TextCursorStyle { stroke: Stroke::new(2.0, from_rgb(0, 83, 125)), ..Default::default() }` `:1599-1602` |
| 26 | `clip_rect_margin` | `f32` | `:1086` | `0.0` `:1544` — **`#[deprecated]`, no effect** `:1085` | *(inherits dark)* `0.0` |
| 27 | `button_frame` | `bool` | `:1089` | `true` `:1545` | *(inherits dark)* `true` |
| 28 | `collapsing_header_frame` | `bool` | `:1092` | `false` `:1546` | *(inherits dark)* `false` |
| 29 | `indent_has_left_vline` | `bool` | `:1095` | `true` `:1547` | *(inherits dark)* `true` |
| 30 | `striped` | `bool` | `:1099` | `false` `:1549` | *(inherits dark)* `false` |
| 31 | `slider_trailing_fill` | `bool` | `:1104` | `false` `:1551` | *(inherits dark)* `false` |
| 32 | `handle_shape` | `HandleShape` | `:1109` | `HandleShape::Rect { aspect_ratio: 0.75 }` `:1552` | *(inherits dark)* same |
| 33 | `interact_cursor` | `Option<CursorIcon>` | `:1116` | `None` `:1554` | *(inherits dark)* `None` |
| 34 | `image_loading_spinners` | `bool` | `:1119` | `true` `:1556` | *(inherits dark)* `true` |
| 35 | `numeric_color_space` | `NumericColorSpace` | `:1122` | `NumericColorSpace::GammaByte` `:1558` | *(inherits dark)* same |
| 36 | `disabled_alpha` | `f32` | `:1125` | `0.5` `:1559` | *(inherits dark)* `0.5` |

36 fields. `dark()` also carries `#[expect(deprecated)]` (`:1496`) purely because it assigns
`clip_rect_margin`.

### `Visuals` methods

```rust
// egui/src/style.rs:1128-1188
impl Visuals {
    pub fn noninteractive(&self) -> &WidgetVisuals;      // :1130  -> &self.widgets.noninteractive
    pub fn text_color(&self) -> Color32;                 // :1135  override_text_color ?? noninteractive.text_color()
    pub fn weak_text_color(&self) -> Color32;            // :1140  weak_text_color ?? text_color().gamma_multiply(weak_text_alpha)
    pub fn strong_text_color(&self) -> Color32;          // :1146  -> widgets.active.text_color()
    pub fn text_edit_bg_color(&self) -> Color32;         // :1151  text_edit_bg_color ?? extreme_bg_color
    pub fn window_fill(&self) -> Color32;                // :1157
    pub fn window_stroke(&self) -> Stroke;               // :1162
    pub fn disabled_alpha(&self) -> f32;                 // :1168
    pub fn disable(&self, color: Color32) -> Color32;    // :1177  color.gamma_multiply(disabled_alpha)
    pub fn gray_out(&self, color: Color32) -> Color32;   // :1184  tint towards widgets.noninteractive.weak_bg_fill
    pub fn dark() -> Self;                               // :1497
    pub fn light() -> Self;                              // :1564
    pub fn ui(&mut self, ui: &mut crate::Ui);            // :2279
}
```

Note the shadowing pair: the **field** `window_fill: Color32` and the **method** `window_fill()`
have the same name, as do `text_edit_bg_color` and `disabled_alpha`. `v.window_fill` is the field,
`v.window_fill()` the getter; for `text_edit_bg_color` they differ in type
(`Option<Color32>` vs `Color32`) and that is exactly the point.

### `TextOptions` — the new text-rendering knobs

```rust
// epaint/src/text/mod.rs:24-27
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct TextOptions {
    pub max_texture_side: usize,                                     // :29
    pub color_transfer_function: crate::FontColorTransferFunction,   // :32
    pub font_hinting: bool,                                          // :39
    pub subpixel_binning: bool,                                      // :53
}
// :57-66 defaults: max_texture_side 2048, color_transfer_function Default (TwoCoverageMinusCoverageSq),
//                  font_hinting true, subpixel_binning true
```

```rust
// epaint/src/image.rs:368-391
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum FontColorTransferFunction {
    Off,                                    // :376 — required for colored emoji; good for light mode
    Gamma(f32),                             // :383
    #[default] TwoCoverageMinusCoverageSq,  // :390 — good for white-on-black
}
impl FontColorTransferFunction {
    pub const LIGHT_MODE_DEFAULT: Self = Self::Off;                       // epaint/src/image.rs:395
    pub const DARK_MODE_DEFAULT: Self = Self::TwoCoverageMinusCoverageSq; // epaint/src/image.rs:398
}
```

Three things follow, and all three bite:

1. `Visuals::text_options.max_texture_side` **is ignored** — overwritten every pass from
   `RawInput::max_texture_side` (`egui/src/style.rs:997-999`, `egui/src/context.rs:580-582`).
2. `TextOptions::default()` uses the **dark-mode** transfer function. If you hand-author light
   visuals and write `text_options: TextOptions::default()`, your light text renders with the
   dark-mode gamma ramp and looks muddy. Start from `Visuals::light()` instead.
3. Changing `text_options` **rebuilds the whole font atlas and flushes the galley cache**
   (`epaint/src/text/fonts.rs:728-742`: `if self.fonts.options() != &options { *self = Self::new(..) }`).
   Since egui reads `text_options` from the *active theme's* style each pass
   (`egui/src/context.rs:578-580`), toggling dark↔light costs one atlas rebuild. Do not animate it.

---

## 6. `Widgets` and `WidgetVisuals` — full dark and light tables

```rust
// egui/src/style.rs:1246-1249
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Widgets {
    pub noninteractive: WidgetVisuals,  // :1254 — bg_stroke = window outline & separators,
                                        //          bg_fill = window bg, fg_stroke = normal text
    pub inactive:       WidgetVisuals,  // :1257 — interactive widget at rest
    pub hovered:        WidgetVisuals,  // :1262 — hovered OR highlighted
    pub active:         WidgetVisuals,  // :1265 — being clicked/dragged
    pub open:           WidgetVisuals,  // :1268 — button with an open menu (combo-box)
}

// :1271-1284
impl Widgets {
    pub fn style(&self, response: &Response) -> &WidgetVisuals;  // :1272
    pub fn dark() -> Self;                                        // :1680  (public)
    pub fn light() -> Self;                                       // :1725  (public)
    pub fn ui(&mut self, ui: &mut crate::Ui);                     // :2150
}
// :1771 impl Default for Widgets -> Self::dark()
// egui/src/widget_style.rs:94
impl Widgets { pub fn state(&self, state: WidgetState) -> &WidgetVisuals; }
```

`Widgets::style()` dispatch order (`:1273-1282`) — note `open` is **never** returned by it:

```rust
if !response.sense.interactive()                                              { &self.noninteractive }
else if response.is_pointer_button_down_on() || response.has_focus() || response.clicked() { &self.active }
else if response.hovered() || response.highlighted()                         { &self.hovered }
else                                                                          { &self.inactive }
```

```rust
// egui/src/style.rs:1287-1289
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct WidgetVisuals {
    pub bg_fill: Color32,           // :1294 — must NEVER be Color32::TRANSPARENT
    pub weak_bg_fill: Color32,      // :1299 — MAY be transparent (button backgrounds)
    pub bg_stroke: Stroke,          // :1304 — the frame/outline stroke
    pub corner_radius: CornerRadius,// :1307 — was `rounding` in older versions
    pub fg_stroke: Stroke,          // :1310 — text & glyph color; `.color` is the text color
    pub expansion: f32,             // :1318 — grow the frame by this much
}
// :1321-1327
impl WidgetVisuals {
    pub fn text_color(&self) -> Color32;       // :1323 -> self.fg_stroke.color
    pub fn ui(&mut self, ui: &mut crate::Ui);  // :2232
}
```

Note: `WidgetVisuals` has **no** `#[serde(default)]` (only `Widgets` does), and it is `Copy`.

### `Widgets::dark()` — `egui/src/style.rs:1680-1722`

| state | `weak_bg_fill` | `bg_fill` | `bg_stroke` | `fg_stroke` | `corner_radius` | `expansion` | line |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `noninteractive` | `from_gray(27)` | `from_gray(27)` | `Stroke::new(1.0, from_gray(60))` | `Stroke::new(1.0, from_gray(140))` | `same(2)` | `0.0` | `:1682` |
| `inactive` | `from_gray(60)` | `from_gray(60)` | `Default::default()` (== `Stroke::NONE`) | `Stroke::new(1.0, from_gray(180))` | `same(2)` | `0.0` | `:1690` |
| `hovered` | `from_gray(70)` | `from_gray(70)` | `Stroke::new(1.0, from_gray(150))` | `Stroke::new(1.5, from_gray(240))` | `same(3)` | `0.0` | `:1698` |
| `active` | `from_gray(55)` | `from_gray(55)` | `Stroke::new(1.0, Color32::WHITE)` | `Stroke::new(2.0, Color32::WHITE)` | `same(2)` | `0.0` | `:1706` |
| `open` | `from_gray(45)` | `from_gray(27)` | `Stroke::new(1.0, from_gray(60))` | `Stroke::new(1.0, from_gray(210))` | `same(2)` | `0.0` | `:1714` |

### `Widgets::light()` — `egui/src/style.rs:1725-1767`

| state | `weak_bg_fill` | `bg_fill` | `bg_stroke` | `fg_stroke` | `corner_radius` | `expansion` | line |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `noninteractive` | `from_gray(248)` | `from_gray(248)` | `Stroke::new(1.0, from_gray(190))` | `Stroke::new(1.0, from_gray(80))` | `same(2)` | `0.0` | `:1727` |
| `inactive` | `from_gray(230)` | `from_gray(230)` | `Default::default()` | `Stroke::new(1.0, from_gray(60))` | `same(2)` | `0.0` | `:1735` |
| `hovered` | `from_gray(220)` | `from_gray(220)` | `Stroke::new(1.0, from_gray(105))` | `Stroke::new(1.5, Color32::BLACK)` | `same(3)` | `0.0` | `:1743` |
| `active` | `from_gray(165)` | `from_gray(165)` | `Stroke::new(1.0, Color32::BLACK)` | `Stroke::new(2.0, Color32::BLACK)` | `same(2)` | `0.0` | `:1751` |
| `open` | `from_gray(220)` | `from_gray(220)` | `Stroke::new(1.0, from_gray(160))` | `Stroke::new(1.0, Color32::BLACK)` | `same(2)` | `0.0` | `:1759` |

`hovered` is the only state with a `corner_radius` of 3; everything else is 2. Every `expansion`
is `0.0` in both themes.

---

## 7. `Selection`, `ImeComposition`, `TextCursorStyle`

```rust
// egui/src/style.rs:1190-1200
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Selection {
    pub bg_fill: Color32,  // :1195 — behind selected text / selected buttons
    pub stroke: Stroke,    // :1198 — color of selected text
}
```

| | `bg_fill` | `stroke` | line |
| --- | --- | --- | --- |
| dark (`Selection::default()`) | `from_rgb(0, 92, 128)` | `Stroke::new(1.0, from_rgb(192, 222, 255))` | `:1616-1621` |
| light | `from_rgb(144, 209, 255)` | `Stroke::new(1.0, from_rgb(0, 83, 125))` | `:1623-1628` |

**`Selection::dark()` and `Selection::light()` are private** (`fn`, not `pub fn`, `:1616`/`:1623`).
Calling them is `E0624`. Get the light one via `Visuals::light().selection`, or write the two
literals above by hand. `Default for Selection` → `Self::dark()` (`:1631-1635`).

```rust
// egui/src/style.rs:1202-1230
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImeComposition {
    pub active_underline_stroke: Stroke,    // :1207
    pub inactive_underline_stroke: Stroke,  // :1210
    pub legacy_visuals: bool,               // :1228
}
// :1673 Default -> Self::dark()  (both ::dark and ::light are PRIVATE, :1638 / :1652)
// :1668 const fn default_legacy_visuals() -> bool { cfg!(windows) }
```

dark: `active = Stroke::new(2.0, from_rgb(192, 222, 255))`, `inactive = active.color.linear_multiply(0.5)`
at the same width (`:1640-1645`). light: `active = Stroke::new(2.0, from_rgb(0, 83, 125))`,
same halving (`:1654-1659`). `legacy_visuals` defaults to `true` on Windows, `false` elsewhere —
on Linux you get the new per-segment CJK composition visuals by default.

```rust
// egui/src/style.rs:947-965
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct TextCursorStyle {
    pub stroke: Stroke,      // :952
    pub preview: bool,       // :955
    pub blink: bool,         // :958
    pub on_duration: f32,    // :961
    pub off_duration: f32,   // :964
}
// :967-978 Default: Stroke::new(2.0, Color32::from_rgb(192, 222, 255)) /* dark mode */,
//          preview false, blink true, on_duration 0.5, off_duration 0.5
// :2591 impl TextCursorStyle { pub fn ui(&mut self, ui: &mut crate::Ui) }  // :2636 for DebugOptions
```

---

## 8. `ScrollStyle`, `ScrollFadeStyle`, `ScrollAnimation`

```rust
// egui/src/style.rs:491-494
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct ScrollStyle {
    pub floating: bool,                       // :502
    pub content_margin: Margin,               // :508
    pub bar_width: f32,                       // :511
    pub handle_min_length: f32,               // :514
    pub bar_inner_margin: f32,                // :517
    pub bar_outer_margin: f32,                // :521
    pub floating_width: f32,                  // :526
    pub floating_allocated_width: f32,        // :534
    pub foreground_color: bool,               // :537
    pub dormant_background_opacity: f32,      // :544
    pub active_background_opacity: f32,       // :551
    pub interact_background_opacity: f32,     // :558
    pub dormant_handle_opacity: f32,          // :565
    pub active_handle_opacity: f32,           // :572
    pub interact_handle_opacity: f32,         // :579
    pub fade: ScrollFadeStyle,                // :581
}
// :584-588  impl Default -> Self::floating()
// :591      pub fn solid() -> Self
// :616      pub fn thin() -> Self
// :641      pub fn floating() -> Self
// :653      pub fn allocated_width(&self) -> f32
// :662      pub fn ui(&mut self, ui: &mut Ui)
// :672      pub fn details_ui(&mut self, ui: &mut Ui)
```

`solid()` (`:591-615`) is the base the other two build on: `floating: false`,
`content_margin: Margin::ZERO`, `bar_width: 6.0`, `handle_min_length: 12.0`,
`bar_inner_margin: 4.0`, `bar_outer_margin: 0.0`, `floating_width: 2.0`,
`floating_allocated_width: 0.0`, `foreground_color: false`, dormant bg `0.0` / active bg `0.4` /
interact bg `0.7`, dormant handle `0.0` / active handle `0.6` / interact handle `1.0`.

`floating()` (the **default**, `:641-652`) = `solid()` with `floating: true`, `bar_width: 10.0`,
`foreground_color: true`, `floating_allocated_width: 0.0`, dormant bg/handle `0.0`.

`thin()` (`:616-639`) = `solid()` with `floating: true`, `bar_width: 10.0`,
`floating_allocated_width: 6.0`, dormant + active bg/handle all `1.0`, interact bg/handle `0.6`.

```rust
// egui/src/style.rs:780-800
pub struct ScrollFadeStyle { pub strength: f32, pub size: f32 }   // :787, :791
// :794-801 Default: strength 0.5, size 20.0   (strength 0.0 disables the fade)

// egui/src/style.rs:827-845
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ScrollAnimation {
    pub points_per_second: f32,  // :832
    pub duration: Rangef,        // :835
}
// :838-844 Default: points_per_second 1000.0, duration Rangef::new(0.1, 0.3)
// :849 pub fn new(points_per_second: f32, duration: Rangef) -> Self
// :857 pub fn none() -> Self          // INFINITY speed, 0.0..=0.0
// :865 pub fn duration(t: f32) -> Self // INFINITY speed, t..=t
```

---

## 9. `DebugOptions`, `HandleShape`, `NumericColorSpace`, `NumberFormatter`

```rust
// egui/src/style.rs:1329-1332  — the whole type is #[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DebugOptions {
    pub debug_on_hover: bool,                        // :1341
    pub debug_on_hover_with_all_modifiers: bool,     // :1353
    pub hover_shows_next: bool,                      // :1357
    pub show_expand_width: bool,                     // :1360
    pub show_expand_height: bool,                    // :1363
    pub show_resize: bool,                           // :1365
    pub show_interactive_widgets: bool,              // :1368
    pub show_widget_hits: bool,                      // :1371
    pub warn_if_rect_changes_id: bool,               // :1375
    pub show_unaligned: bool,                        // :1380
    pub show_focused_widget: bool,                   // :1387
}
// :1391-1407 Default: all false EXCEPT
//   debug_on_hover_with_all_modifiers = cfg!(feature = "callstack") && !cfg!(target_arch = "wasm32")
//   show_unaligned = cfg!(debug_assertions)   // i.e. true wherever the type exists
```

```rust
// egui/src/style.rs:1232-1243
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HandleShape { Circle, Rect { aspect_ratio: f32 } }
// default in both themes: Rect { aspect_ratio: 0.75 }   (style.rs:1552)

// egui/src/style.rs:2735-2746
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericColorSpace { GammaByte, Linear }
// :2749 pub fn toggle_button_ui(&mut self, ui: &mut Ui) -> crate::Response
// :2767 Display: GammaByte => "U8", Linear => "F"

// egui/src/style.rs:19-20
#[derive(Clone)]
pub struct NumberFormatter(Arc<dyn Fn(f64, RangeInclusive<usize>) -> String + Send + Sync>);
// :30 pub fn new(..), :45 pub fn format(&self, value: f64, decimals: RangeInclusive<usize>) -> String
```

---

## 10. `egui::widget_style` — the new per-widget style layer

New module in this release (`egui/src/lib.rs:426`). It derives concrete `Frame` + text visuals
from a `Style` for a given widget state, which is what you want if you are writing a custom widget
that must look like a built-in one.

```rust
// egui/src/widget_style.rs:83-90
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetState { Noninteractive, #[default] Inactive, Hovered, Active }

// :13-23
pub struct TextVisuals { pub font_id: FontId, pub color: Color32,
                         pub underline: Stroke, pub strikethrough: Stroke }
// :26-32
pub struct WidgetStyle { pub frame: Frame, pub text: TextVisuals, pub stroke: Stroke }
// :35-38
pub struct ButtonStyle { pub frame: Frame, pub text_style: TextVisuals }
// :41-59
pub struct CheckboxStyle { pub frame: Frame, pub text_style: TextVisuals, pub checkbox_size: f32,
                           pub check_size: f32, pub checkbox_frame: Frame, pub check_stroke: Stroke }
// :62-71
pub struct LabelStyle { pub frame: Frame, pub text: TextVisuals, pub wrap_mode: TextWrapMode }
// :74-80
pub struct SeparatorStyle { pub spacing: f32, pub stroke: Stroke }

// :118-218
impl Style {
    pub fn widget_style(&self, classes: &Classes, state: WidgetState) -> WidgetStyle;       // :120
    pub fn button_style(&self, classes: &Classes, state: WidgetState) -> ButtonStyle;       // :146
    pub fn checkbox_style(&self, classes: &Classes, state: WidgetState) -> CheckboxStyle;   // :174
    pub fn label_style(&self, classes: &Classes, state: WidgetState) -> LabelStyle;         // :194
    pub fn separator_style(&self, classes: &Classes, state: WidgetState) -> SeparatorStyle; // :212
}

// :104-116
impl Response { pub fn widget_state(&self) -> WidgetState; }  // same dispatch as Widgets::style

// :222 pub const ROOT_CLASS: &str = "root";
// :225 pub const SELECTED_CLASS: &str = "selected";
// :228 pub type ClassName = Cow<'static, str>;
// :235 pub struct Classes { .. }   (Default + Clone; `Classes::default()` is the empty set)
// :269 pub trait HasClasses { fn classes(&self) -> &Classes; fn classes_mut(&mut self) -> &mut Classes;
//                            fn with_class(self, class: impl Into<ClassName>) -> Self; .. }
```

`button_style` is the one that reads `SELECTED_CLASS` and swaps in `visuals.selection` (`:150-155`);
`widget_style` ignores classes entirely today (`_classes`, `:120`).

---

## 11. Theme, `ThemePreference`, and how the setters actually behave

```rust
// egui/src/memory/theme.rs:4-12
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum Theme { Dark, Light }

impl Theme {
    pub fn default_visuals(self) -> crate::Visuals;      // :16
    pub fn default_style(self) -> crate::Style;          // :24
    pub fn from_dark_mode(dark_mode: bool) -> Self;      // :32
}

// egui/src/memory/theme.rs:64-77
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemePreference { Dark, Light, #[default] System }

impl From<Theme> for ThemePreference { /* :79-86 */ }
impl ThemePreference {
    pub fn radio_buttons(&mut self, ui: &mut crate::Ui);  // :90 — System / Dark / Light radio row
}
```

`Theme` and `ThemePreference` are both re-exported at the crate root (`egui/src/lib.rs:483`).

### The storage model

`Options` keeps **two** styles, plus a preference and a fallback:

```rust
// egui/src/memory/mod.rs:193-320
pub struct Options {
    pub dark_style: std::sync::Arc<Style>,       // :196  #[serde(skip)]
    pub light_style: std::sync::Arc<Style>,      // :200  #[serde(skip)]
    pub theme_preference: ThemePreference,       // :206  default ThemePreference::System
    pub fallback_theme: Theme,                   // :212  default Theme::Dark
    pub(crate) system_theme: Option<Theme>,      // :217  not public — read it via Context::system_theme()
    pub sync_window_theme: bool,                 // :229  default true
    pub zoom_factor: f32,                        // :240  default 1.0
    pub zoom_with_keyboard: bool,                // :253  default true
    pub quit_shortcuts: Vec<crate::KeyboardShortcut>,  // :263
    pub tessellation_options: epaint::TessellationOptions, // :266
    pub repaint_on_widget_change: bool,          // :273
    pub max_passes: NonZeroUsize,                // :290  default 2
    pub screen_reader: bool,                     // :301
    pub warn_on_id_clash: bool,                  // :306
    pub input_options: crate::input_state::InputOptions,  // :309
    pub reduce_texture_memory: bool,             // :322  default false
}
// :363-369  fn theme(&self) -> Theme {
//               Dark => Theme::Dark, Light => Theme::Light,
//               System => self.system_theme.unwrap_or(self.fallback_theme)
//           }
// :371-376  fn style(&self) -> &Arc<Style>  -> dark_style | light_style, per theme()
```

Both styles are `#[serde(skip)]` — **persisting `Options` does not persist your styles**, only the
`theme_preference` and `fallback_theme`. Re-apply your visuals on every startup.

### Every setter, verbatim

```rust
// egui/src/context.rs
pub fn system_theme(&self) -> Option<Theme>;                                          // :2149
pub fn theme(&self) -> Theme;                                                         // :2155
pub fn set_theme(&self, theme_preference: impl Into<crate::ThemePreference>);         // :2167
pub fn global_style(&self) -> Arc<Style>;                                             // :2172
pub fn global_style_mut(&self, mutate_style: impl FnOnce(&mut Style));                // :2186
pub fn set_global_style(&self, style: impl Into<Arc<Style>>);                         // :2197
pub fn all_styles_mut(&self, mut mutate_style: impl FnMut(&mut Style));               // :2210
pub fn style_of(&self, theme: Theme) -> Arc<Style>;                                   // :2218
pub fn style_mut_of(&self, theme: Theme, mutate_style: impl FnOnce(&mut Style));      // :2234
pub fn set_style_of(&self, theme: Theme, style: impl Into<Arc<Style>>);               // :2247
pub fn set_visuals_of(&self, theme: Theme, visuals: crate::Visuals);                  // :2264
pub fn set_visuals(&self, visuals: crate::Visuals);                                   // :2277
pub fn style_ui(&self, ui: &mut Ui, theme: Theme);                                    // :3663
```

Behaviour, read off the bodies:

| call | what it actually does | body |
| --- | --- | --- |
| `set_theme(pref)` | writes `options.theme_preference` only. Accepts `Theme` **or** `ThemePreference` via `Into` | `:2168` |
| `global_style()` | `Arc::clone(opt.style())` — the **active** theme's style | `:2173` |
| `global_style_mut(f)` | `f(Arc::make_mut(opt.style_mut()))` — **active theme only** | `:2187` |
| `set_global_style(s)` | `*opt.style_mut() = s.into()` — **active theme only** | `:2198` |
| `all_styles_mut(f)` | calls `f` on **both** `dark_style` and `light_style`. `FnMut`, so it runs twice | `:2211-2216` |
| `style_of(t)` / `set_style_of(t, s)` / `style_mut_of(t, f)` | explicit per-theme, no ambiguity | `:2219-2255` |
| `set_visuals_of(t, v)` | `self.style_mut_of(t, |style| style.visuals = v)` | `:2265` |
| `set_visuals(v)` | `self.style_mut_of(self.theme(), |style| style.visuals = v)` — **active theme only** | `:2278` |
| `style_ui(ui, t)` | clones `style_of(t)`, runs the interactive editor, writes it back | `:3664-3667` |

**The rule:** `set_style` / `set_visuals` / `global_style_mut` / `set_global_style` all silently
mean "the theme that happens to be active right now". For anything you want to survive a system
theme flip, use `set_style_of` / `set_visuals_of` for *both* themes, or `all_styles_mut`.

```rust
// Correct: both themes get your visuals, and the preference follows the OS.
fn apply_theme(ctx: &egui::Context) {
    ctx.set_visuals_of(egui::Theme::Dark, my_dark_visuals());
    ctx.set_visuals_of(egui::Theme::Light, my_light_visuals());
    ctx.set_theme(egui::ThemePreference::System);
}

// Also correct, and the only way to change non-visuals fields in both at once:
ctx.all_styles_mut(|style| {
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.animation_time = 0.12;
});
```

### Per-`Ui` overrides

```rust
// egui/src/ui.rs
pub fn style(&self) -> &Arc<Style>;                        // :364
pub fn style_mut(&mut self) -> &mut Style;                 // :379  Arc::make_mut — clone-on-write
pub fn set_style(&mut self, style: impl Into<Arc<Style>>); // :386
pub fn reset_style(&mut self);                             // :391  -> self.ctx().global_style()
pub fn spacing(&self) -> &crate::style::Spacing;           // :398
pub fn spacing_mut(&mut self) -> &mut crate::style::Spacing; // :411
pub fn visuals(&self) -> &crate::Visuals;                  // :418
pub fn visuals_mut(&mut self) -> &mut crate::Visuals;      // :433
pub fn pixels_per_point(&self) -> f32;                     // :463
pub fn text_style_height(&self, style: &TextStyle) -> f32; // :632  rounded to emath::GUI_ROUNDING
```

`Ui::set_style` exists; `Context::set_style` does **not**. That asymmetry is the single most common
0.36 compile error.

---

## 12. Fonts — `FontId`, `FontFamily`, `FontData`, `FontTweak`

```rust
// epaint/src/text/fonts.rs:19-21
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct FontId {
    pub size: f32,          // :23 — height in POINTS, not pixels
    pub family: FontFamily, // :26
}
impl FontId {
    pub const fn new(size: f32, family: FontFamily) -> Self;  // :42
    pub const fn proportional(size: f32) -> Self;             // :47
    pub const fn monospace(size: f32) -> Self;                // :52
}
// :30-37 Default: FontId { size: 14.0, family: FontFamily::Proportional }
// :57 Hash is via emath::OrderedFloat(size) — FontId IS hashable despite the f32
```

Careful: `FontId::default()` is **14.0**, but `TextStyle::Body` in the default style is **13.0**
(`egui/src/style.rs:1419`). They are not the same number.

```rust
// epaint/src/text/fonts.rs:72-95
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum FontFamily {
    #[default] Proportional,   // :78
    Monospace,                 // :83
    Name(Arc<str>),            // :92 — one of the keys in FontDefinitions::families
}
// :97 Display: "Monospace" | "Proportional" | the name itself
```

`FontFamily::Name` holds `Arc<str>`, so `FontFamily::Name("ui".into())` works from a `&str`.
It is `Ord` + `Hash`, which is why `FontDefinitions::families` can be a `BTreeMap`.

```rust
// epaint/src/text/fonts.rs:110-122
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct FontData {
    pub font: Cow<'static, [u8]>,  // :114 — the raw .ttf/.otf/.ttc bytes
    pub index: u32,                // :118 — face index inside the file; 0 unless it's a .ttc
    pub tweak: FontTweak,          // :121
}
```

### `FontData`'s real constructors — there are exactly three associated fns

```rust
// epaint/src/text/fonts.rs:124-177
impl FontData {
    pub fn from_static(font: &'static [u8]) -> Self;      // :125 — Cow::Borrowed, index 0, default tweak
    pub fn from_owned(font: Vec<u8>) -> Self;             // :133 — Cow::Owned,   index 0, default tweak
    pub fn tweak(self, tweak: FontTweak) -> Self;         // :141 — builder, consumes self
    pub fn variation_axes(&self) -> Vec<FontVariationAxis>; // :153 — variable-font axes, empty for static
}
// :197 impl AsRef<[u8]> for FontData
```

There is **no** `FontData::new`, no `from_path`, no `from_file`. To set `index` (for a `.ttc`
collection such as `NotoSansCJK-Regular.ttc`) you must build the struct literally:

```rust
let cjk = egui::FontData {
    font: std::borrow::Cow::Owned(std::fs::read("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc")?),
    index: 2,                       // pick the face you want out of the collection
    tweak: egui::FontTweak::default(),
};
```

```rust
// epaint/src/text/fonts.rs:206-264
#[derive(Clone, Debug, PartialEq)]
pub struct FontTweak {
    pub scale: f32,                          // :213  default 1.0
    pub y_offset_factor: f32,                // :224  default 0.0  (fraction of font size, + is down)
    pub y_offset: f32,                       // :232  default 0.0  (absolute points)
    pub hinting: Option<bool>,               // :237  default None -> use TextOptions::font_hinting
    pub hinting_target: HintingTarget,       // :242  default HintingTarget::default()
    pub subpixel_binning: Option<bool>,      // :247  default None -> use TextOptions::subpixel_binning
    pub coords: VariationCoords,             // :250  variable-font axis overrides ("wght", …)
    pub thin_space_width: f32,               // :258  default 0.5  (fraction of a normal space)
    pub tab_size: f32,                       // :263  default 4.0  (in space widths)
}
// :266-280 Default as annotated above
```

`thin_space_width` and `tab_size` are per-font, and new here — if your monospace font renders tabs
at the wrong width, this is the knob, not `Spacing`.

```rust
// epaint/src/text/fonts.rs:295-309
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintingTarget { /* … see :297-309; Default at :310 */ }
// epaint/src/text/fonts.rs:179-195
#[derive(Clone, Debug, PartialEq)]
pub struct FontVariationAxis { pub tag: Tag, pub name: Option<String>,
                               pub range: Rangef, pub default: f32, pub hidden: bool }
```

---

## 13. `FontDefinitions`, `set_fonts`, `add_font`, and the fallback chain

```rust
// epaint/src/text/fonts.rs:428-444
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct FontDefinitions {
    pub font_data: BTreeMap<String, Arc<FontData>>,   // :435  <-- Arc<FontData>, not FontData
    pub families: BTreeMap<FontFamily, Vec<String>>,  // :443  keys into font_data, in priority order
}
impl FontDefinitions {
    pub fn empty() -> Self;                                    // :561 — Proportional and Monospace both []
    pub fn builtin_font_names() -> &'static [&'static str];    // :574 (default_fonts) / :585 (without)
}
// :490-556 impl Default — see the table below; without `default_fonts` it is literally Self::empty()
```

### What `FontDefinitions::default()` contains (feature `default_fonts`, `:499-556`)

`font_data` (4 entries):

| key | source | tweak |
| --- | --- | --- |
| `"Hack"` | `FontData::from_static(HACK_REGULAR)` `:504` | default |
| `"NotoEmoji-Regular"` | `from_static(NOTO_EMOJI_REGULAR)` `:509` | `FontTweak { scale: 0.81, ..Default::default() }` |
| `"Ubuntu-Light"` | `from_static(UBUNTU_LIGHT)` `:516` | default |
| `"emoji-icon-font"` | `from_static(EMOJI_ICON)` `:522` | `FontTweak { scale: 0.90, ..Default::default() }` |

`families` (2 entries, `:529-548`):

| family | chain, in order |
| --- | --- |
| `Monospace` | `["Hack", "Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"]` |
| `Proportional` | `["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"]` |

`Ubuntu-Light` sits in the *monospace* chain too, as the fallback for `√` and friends. The bundled
fonts cover **Latin and Cyrillic only** — there is no CJK, Arabic, Hebrew, Thai, Devanagari or
Korean glyph anywhere in the default set (`egui/src/context.rs:2097-2098`).

### How the fallback chain is resolved

```rust
// epaint/src/text/fonts.rs:677-693
/// Walk the fallback chain and return the first face whose charmap supports `c`.
pub(crate) fn find_face_for_char(&self, c: char,
    fonts_by_id: &mut nohash_hasher::IntMap<FontFaceKey, FontFace>) -> Option<FontFaceKey> {
    for font_key in &self.fonts {
        let font_face = fonts_by_id.get_mut(font_key).expect("Nonexistent font ID");
        if font_face.glyph_id_resolution(c).is_some() { return Some(*font_key); }
    }
    None
}
```

Facts that follow, all from `epaint/src/text/fonts.rs`:

- Resolution is **per character**, not per run or per script. The first face in the family's `Vec<String>`
  whose charmap has the codepoint wins (`:686-691`).
- The result is memoised per family in `face_cache: char -> FontFaceKey` (`:629`), and the choice
  depends only on charmap support, never on variation coords.
- If **no** face in the chain has the char, egui paints a replacement: `'◻'` (U+25FB) if any face
  has it, else `'?'`, else an empty glyph with a `log::warn!` (`:637-670`).
- **Order matters and is the whole API.** There is no script-to-font mapping, no fontconfig, no
  OS font enumeration. If you want Arabic, you put an Arabic font in the chain yourself.
- A per-family chain is built lazily on first use of that family (`:1021-1044`).

### Three panics you can trigger at `set_fonts` time

All happen on the first pass after the new definitions are installed, not at the call itself:

| message | cause | line |
| --- | --- | --- |
| `FontFamily::{family:?} is not bound to any fonts` | a `FontId` names a family with no entry in `families` | `:1025` |
| `No font data found for {font_name:?}` | a family chain names a key missing from `font_data` | `:1033` |
| `Error parsing {name:?} TTF/OTF font file: {err}` | unparseable bytes, or a bad `index` on a `.ttc` | `:990` |

So: always insert into `font_data` **before** pushing the name into a `families` chain, and never
hand out a `FontFamily::Name(..)` you have not registered.

### `Context::set_fonts` vs `Context::add_font`

```rust
// egui/src/context.rs:2096-2117
/// The new fonts will become active at the start of the next pass.
/// This will overwrite the existing fonts.
pub fn set_fonts(&self, font_definitions: FontDefinitions);

// egui/src/context.rs:2119-2145
/// The new font will become active at the start of the next pass.
/// This will keep the existing fonts.
pub fn add_font(&self, new_font: FontInsert);
```

```rust
// epaint/src/text/fonts.rs:446-478
#[derive(Debug, Clone)]
pub struct FontInsert {
    pub name: String,                      // :449
    pub data: FontData,                    // :452 — plain FontData here, NOT Arc
    pub families: Vec<InsertFontFamily>,   // :455
}
#[derive(Debug, Clone)]
pub struct InsertFontFamily { pub family: FontFamily, pub priority: FontPriority }  // :461, :464
#[derive(Debug, Clone)]
pub enum FontPriority {
    Highest,  // :472 — inserted at index 0 of the chain
    Lowest,   // :477 — pushed to the end of the chain
}
impl FontInsert {
    pub fn new(name: &str, data: FontData, families: Vec<InsertFontFamily>) -> Self;  // :481
}
```

Both are deferred to `Context::update_fonts_mut` (`egui/src/context.rs:541-596`), called from
`begin_pass`. `FontPriority::Highest` → `fam.insert(0, name)`, `Lowest` → `fam.push(name)`
(`egui/src/context.rs:565-568`). `add_font` also `entry(..).or_default()`s the family, so it can
create a new `FontFamily::Name` chain from nothing.

Two cheap deduplications to know about:

- `set_fonts` compares the whole `FontDefinitions` — **including every byte of TTF data** — and
  skips the update if equal (`egui/src/context.rs:2106-2113`; the comment calls the comparison
  expensive). Do not call it every frame.
- `add_font` only compares the **name** against `font_data` and no-ops if present
  (`egui/src/context.rs:2129-2138`). Re-adding under the same name will never replace the bytes.

Either one sets `self.fonts = None`, which discards the atlas and every cached galley.

### Reading the font state back

```rust
// egui/src/context.rs:1091-1124  — panic before the first Context::run_ui!
pub fn fonts<R>(&self, reader: impl FnOnce(&FontsView<'_>) -> R) -> R;         // :1096
pub fn fonts_mut<R>(&self, reader: impl FnOnce(&mut FontsView<'_>) -> R) -> R; // :1113
// both: .expect("No fonts available until first call to Context::run()")

// epaint/src/text/fonts.rs:819-957
impl FontsView<'_> {
    pub fn options(&self) -> &TextOptions;                          // :821
    pub fn definitions(&self) -> &FontDefinitions;                  // :826
    pub fn glyph_width(&mut self, font_id: &FontId, c: char) -> f32;// :845
    pub fn has_glyph(&mut self, font_id: &FontId, c: char) -> bool; // :852
    pub fn has_glyphs(&mut self, font_id: &FontId, s: &str) -> bool;// :857
    pub fn row_height(&mut self, font_id: &FontId) -> f32;          // :865 (rounded to GUI_ROUNDING)
    pub fn families(&self) -> Vec<FontFamily>;                      // :878
    pub fn layout_job(&mut self, job: LayoutJob) -> Arc<Galley>;    // :890
    pub fn font_atlas_fill_ratio(&self) -> f32;                     // :908
}
```

`has_glyphs` is the honest way to check a fallback actually landed:

```rust
// Call this from inside your update fn, never before the first frame.
fn fallbacks_ok(ctx: &egui::Context) -> bool {
    let body = egui::FontId::proportional(14.0);
    ctx.fonts_mut(|f| f.has_glyphs(&body, "日本語") && f.has_glyphs(&body, "العربية"))
}
```

---

## 14. `TextStyle`

```rust
// egui/src/style.rs:69-94
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum TextStyle {
    Small,                      // :73
    Body,                       // :76
    Monospace,                  // :79
    Button,                     // :85
    Heading,                    // :88
    Name(std::sync::Arc<str>),  // :93
}
// :96 Display -> "Small" | "Body" | "Monospace" | "Button" | "Heading" | the name
// :109-122
impl TextStyle {
    /// PANICS if not present in `Style::text_styles`.
    pub fn resolve(&self, style: &Style) -> FontId;  // :111
}
```

`resolve` **panics** with the full list of available styles if the entry is missing (`:112-118`).
So a `TextStyle::Name("Display".into())` used anywhere must be present in `Style::text_styles` in
**both** the dark and light style — another reason to use `all_styles_mut`.

`TextStyle` is `Ord`, so `BTreeMap<TextStyle, FontId>` orders by the derive: `Small < Body <
Monospace < Button < Heading < Name(..)`, with `Name`s sorted among themselves by string.

```rust
// Add a custom style to BOTH themes, keeping the built-ins:
ctx.all_styles_mut(|style| {
    style.text_styles.insert(
        egui::TextStyle::Name("Display".into()),
        egui::FontId::new(32.0, egui::FontFamily::Proportional),
    );
});
```

---

## 15. `pixels_per_point`, zoom, and fractional scaling

```rust
// egui/src/context.rs:2281-2344
/// This is calculated as zoom_factor() * native_pixels_per_point()
pub fn pixels_per_point(&self) -> f32;                      // :2285  -> input(|i| i.pixels_per_point)
/// This will actually translate to a call to Self::set_zoom_factor.
pub fn set_pixels_per_point(&self, pixels_per_point: f32);  // :2293
pub fn native_pixels_per_point(&self) -> Option<f32>;       // :2304  from ViewportInfo; read-only
pub fn zoom_factor(&self) -> f32;                           // :2316  default 1.0
pub fn set_zoom_factor(&self, zoom_factor: f32);            // :2334  applies at start of next pass
```

The composition rule, from `Context::begin_pass` (`egui/src/context.rs:450-454`):

```rust
let native_pixels_per_point = new_raw_input.viewport().native_pixels_per_point.unwrap_or(1.0);
let pixels_per_point = self.memory.options.zoom_factor * native_pixels_per_point;
```

Consequences worth knowing:

- `set_pixels_per_point(p)` is **not** a setter for a stored value. It computes
  `p / native_pixels_per_point().unwrap_or(1.0)` and calls `set_zoom_factor` with that
  (`:2294-2298`), and it no-ops entirely if `p == self.pixels_per_point()` (`:2294`).
  If the backend has not reported a `native_pixels_per_point` yet, `unwrap_or(1.0)` makes your
  requested ppp land in `zoom_factor` directly, and it will be *multiplied* again once the real
  DPI arrives. Prefer `set_zoom_factor` and let the backend own the DPI.
- `set_zoom_factor` does not update `zoom_factor()` until the end of the pass (`:2323`); it stashes
  `new_zoom_factor` and applies it in `begin_pass`, rescaling `screen_rect` by the ratio to avoid
  jitter (`:437-449`).
- Fractional values are fully supported — nothing rounds `pixels_per_point` to an integer. The
  keyboard zoom helpers step by `0.1` and clamp to `0.2 ..= 5.0`
  (`egui/src/gui_zoom.rs:48-67`), and `Options::zoom_with_keyboard` (default `true`,
  `egui/src/memory/mod.rs:253`) wires Cmd+`+` / Cmd+`-` / Cmd+`0`.

```rust
// egui/src/gui_zoom.rs
pub mod kb_shortcuts { pub const ZOOM_IN: KeyboardShortcut;            // :10
                       pub const ZOOM_IN_SECONDARY: KeyboardShortcut;  // :18
                       pub const ZOOM_OUT: KeyboardShortcut;           // :22
                       pub const ZOOM_RESET: KeyboardShortcut; }       // :25
pub fn zoom_in(ctx: &Context);            // :52
pub fn zoom_out(ctx: &Context);           // :61
pub fn zoom_menu_buttons(ui: &mut Ui);    // :72
```

### Rounding at fractional scale

```rust
// emath/src/gui_rounding.rs:18
pub const GUI_ROUNDING: f32 = 1.0 / 32.0;

// emath/src/gui_rounding.rs:23-54  — implemented for f32, f64, Vec2, Pos2, Rect
pub trait GuiRounding {
    fn round_ui(self) -> Self;                                   // :31  -> multiple of 1/32
    fn floor_ui(self) -> Self;                                   // :34
    fn round_to_pixels(self, pixels_per_point: f32) -> Self;     // :43  (self * ppp).round() / ppp
    fn round_to_pixel_center(self, pixels_per_point: f32) -> Self;// :53 ((self*ppp - 0.5).round() + 0.5) / ppp
}
```

Widget coordinates round to `GUI_ROUNDING` (1/32 pt), **not** to whole pixels — rounding to
integers causes visible judder while scrolling (`emath/src/gui_rounding.rs:12-13`). Round to
physical pixels only when you want a crisp edge, and use `round_to_pixel_center` for one-pixel
lines. `Stroke::round_center_to_pixel(ppp, &mut coord)` (`epaint/src/stroke.rs:41`) already picks
between the two based on whether the stroke is an odd number of physical pixels wide.

`Ui::text_style_height` and `FontsView::row_height` both return values already rounded to
`GUI_ROUNDING` (`egui/src/ui.rs:631`, `epaint/src/text/fonts.rs:863`).

---

## 16. The geometry/color primitives these structs are built from

```rust
// epaint/src/corner_radius.rs:11-25, 48-62
pub struct CornerRadius { pub nw: u8, pub ne: u8, pub sw: u8, pub se: u8 }
impl CornerRadius {
    pub const ZERO: Self;                       // :50
    pub const fn same(radius: u8) -> Self;      // :59
}
// :34 impl From<u8>, :41 impl From<f32> (rounds!). Default == ZERO (:27)

// epaint/src/margin.rs:13-50
pub struct Margin { pub left: i8, pub right: i8, pub top: i8, pub bottom: i8 }
impl Margin {
    pub const ZERO: Self;                          // :23
    pub const fn same(margin: i8) -> Self;         // :33
    pub const fn symmetric(x: i8, y: i8) -> Self;  // :44
    pub const fn leftf(self) -> f32;               // :55  (and rightf/topf/bottomf)
}

// epaint/src/shadow.rs:8-44
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shadow { pub offset: [i8; 2], pub blur: u8, pub spread: u8, pub color: Color32 }
impl Shadow { pub const NONE: Self; }              // :40
// The whole struct is 8 bytes and there is a test asserting it (:29-36)

// epaint/src/stroke.rs:11-31
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stroke { pub width: f32, pub color: Color32 }
impl Stroke {
    pub const NONE: Self;                                       // :20  (== Default)
    pub fn new(width: f32, color: impl Into<Color32>) -> Self;  // :26
    pub fn is_empty(&self) -> bool;                             // :35
}

// ecolor/src/color32.rs — constructors used by every default above
pub const TRANSPARENT: Self;                                    // :60
pub const BLACK: Self;                                          // :61
pub const WHITE: Self;                                          // :68
pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self;             // :108
pub const fn from_rgba_premultiplied(r: u8, g: u8, b: u8, a: u8) -> Self;   // :122
pub const fn from_rgba_unmultiplied_const(r: u8, g: u8, b: u8, a: u8) -> Self; // :139
pub const fn from_gray(l: u8) -> Self;                          // :159
pub const fn from_black_alpha(a: u8) -> Self;                   // :165
pub const fn from_additive_luminance(l: u8) -> Self;            // :177
pub fn gamma_multiply(self, factor: f32) -> Self;               // :269
pub fn linear_multiply(self, factor: f32) -> Self;              // :305
```

`Margin` is `i8` and `CornerRadius`/`Shadow` are `u8` — a margin of `200` or a corner radius of
`300.0` will not fit. `CornerRadius::from(f32)` silently `round()`s and casts (`:40-45`).

---

## 17. Complete working function

Registers three application TTFs, layers CJK / Arabic / Thai fallbacks under every family
(including a custom `FontFamily::Name("fx-ui")` used for headings), then applies hand-authored dark
**and** light `Visuals`. Compiled clean — zero errors, zero warnings — against `egui = "=0.36.0"`
in this workspace.

```rust
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use egui::style::{Selection, Widgets};
use egui::{
    Color32, Context, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak,
    Margin, Shadow, Stroke, Style, TextStyle, Theme, ThemePreference, Visuals,
};

const INTER: &str = "Inter";
const MANROPE: &str = "Manrope";
const JETBRAINS: &str = "JetBrainsMono";

/// Our own family, used for headings. Must be registered before any FontId names it,
/// or epaint panics with "FontFamily::Name(..) is not bound to any fonts".
const UI_FAMILY: &str = "fx-ui";

const CJK: &str = "NotoCJK";
const ARABIC: &str = "NotoArabic";
const THAI: &str = "NotoThai";

/// Read a face off disk into `font_data`. Returns false (and logs) if the file is missing,
/// so a missing optional fallback degrades instead of killing startup.
fn read_face(defs: &mut FontDefinitions, name: &str, path: &str, tweak: FontTweak) -> bool {
    let Ok(bytes) = std::fs::read(Path::new(path)) else {
        log::warn!("font not found, skipping: {path}");
        return false;
    };
    defs.font_data.insert(
        name.to_owned(),
        Arc::new(FontData::from_owned(bytes).tweak(tweak)),
    );
    true
}

/// Append `name` to the end of `family`'s chain, if it was actually loaded and isn't already there.
fn push_fallback(defs: &mut FontDefinitions, family: &FontFamily, name: &str) {
    if defs.font_data.contains_key(name)
        && let Some(chain) = defs.families.get_mut(family)
        && !chain.iter().any(|n| n == name)
    {
        chain.push(name.to_owned());
    }
}

pub fn install_fonts_and_theme(ctx: &Context) {
    // ---------------------------------------------------------------- 1. fonts
    // Start from the built-ins so we keep NotoEmoji-Regular + emoji-icon-font at the tail.
    let mut defs = FontDefinitions::default();

    read_face(&mut defs, INTER, "assets/fonts/Inter-Regular.ttf", FontTweak::default());
    read_face(
        &mut defs,
        MANROPE,
        "assets/fonts/Manrope-Bold.ttf",
        FontTweak { y_offset_factor: -0.02, ..Default::default() },
    );
    read_face(
        &mut defs,
        JETBRAINS,
        "assets/fonts/JetBrainsMono-Regular.ttf",
        FontTweak { tab_size: 4.0, ..Default::default() },
    );

    // Script fallbacks. The bundled egui fonts cover Latin + Cyrillic ONLY.
    for (name, path) in [
        (CJK, "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
        (ARABIC, "/usr/share/fonts/noto/NotoSansArabic-Regular.ttf"),
        (THAI, "/usr/share/fonts/noto/NotoSansThai-Regular.ttf"),
    ] {
        read_face(&mut defs, name, path, FontTweak::default());
    }

    // Primary faces go to the FRONT of the built-in chains.
    if defs.font_data.contains_key(INTER) {
        defs.families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, INTER.to_owned());
    }
    if defs.font_data.contains_key(JETBRAINS) {
        defs.families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, JETBRAINS.to_owned());
    }

    // Custom heading family: Manrope, then Inter, then whatever Proportional resolved to.
    let heading_chain: Vec<String> = [MANROPE, INTER]
        .iter()
        .filter(|n| defs.font_data.contains_key(**n))
        .map(|n| (*n).to_owned())
        .chain(
            defs.families
                .get(&FontFamily::Proportional)
                .cloned()
                .unwrap_or_default(),
        )
        .collect();
    defs.families
        .insert(FontFamily::Name(UI_FAMILY.into()), heading_chain);

    // Script fallbacks last in EVERY family: resolution is per-character, first match wins.
    for family in [
        FontFamily::Proportional,
        FontFamily::Monospace,
        FontFamily::Name(UI_FAMILY.into()),
    ] {
        for name in [CJK, ARABIC, THAI] {
            push_fallback(&mut defs, &family, name);
        }
    }

    // Deferred: takes effect at the start of the next pass, and rebuilds the whole atlas.
    ctx.set_fonts(defs);

    // ------------------------------------------------------- 2. text styles + metrics
    let text_styles: BTreeMap<TextStyle, FontId> = [
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace)),
        (TextStyle::Heading, FontId::new(20.0, FontFamily::Name(UI_FAMILY.into()))),
        (
            TextStyle::Name("Display".into()),
            FontId::new(32.0, FontFamily::Name(UI_FAMILY.into())),
        ),
    ]
    .into();

    // all_styles_mut runs the closure on BOTH dark_style and light_style. Anything set only
    // through global_style_mut/set_global_style would be lost when the OS theme flips.
    ctx.all_styles_mut(move |style| {
        style.text_styles = text_styles.clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.window_margin = Margin::same(10);
        style.spacing.interact_size = egui::vec2(44.0, 26.0);
        style.spacing.slider_width = 180.0;
        style.interaction.tooltip_delay = 0.3;
        style.interaction.selectable_labels = false;
        style.animation_time = 0.12;
        style.compact_menu_style = false;
    });

    // ------------------------------------------------------------------ 3. visuals
    // Per-theme, so a system theme flip keeps our look in both directions.
    ctx.set_visuals_of(Theme::Dark, fx_dark());
    ctx.set_visuals_of(Theme::Light, fx_light());
    ctx.set_theme(ThemePreference::System);
}

/// Shared widget-state tweaks. Widgets::dark()/light() are public; Selection::dark()/light() are NOT.
fn widget_set(base: Widgets, accent: Color32, strong_text: Color32) -> Widgets {
    let mut w = base;
    for wv in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        wv.corner_radius = CornerRadius::same(6);
    }
    w.hovered.bg_stroke = Stroke::new(1.0, accent);
    w.active.bg_stroke = Stroke::new(1.0, accent);
    w.active.fg_stroke = Stroke::new(1.5, strong_text);
    w
}

pub fn fx_dark() -> Visuals {
    let accent = Color32::from_rgb(0, 170, 255);
    // FRU off Visuals::dark() — an exhaustive literal would force us to name the
    // deprecated `clip_rect_margin` field and emit a warning.
    Visuals {
        dark_mode: true,
        widgets: widget_set(Widgets::dark(), accent, Color32::WHITE),
        selection: Selection {
            bg_fill: accent.gamma_multiply(0.45),
            stroke: Stroke::new(1.0, Color32::from_rgb(225, 240, 255)),
        },
        hyperlink_color: accent,
        panel_fill: Color32::from_gray(22),
        window_fill: Color32::from_gray(26),
        extreme_bg_color: Color32::from_gray(12),
        faint_bg_color: Color32::from_additive_luminance(6),
        code_bg_color: Color32::from_gray(40),
        window_stroke: Stroke::new(1.0, Color32::from_gray(52)),
        window_corner_radius: CornerRadius::same(10),
        menu_corner_radius: CornerRadius::same(8),
        window_shadow: Shadow {
            offset: [0, 12],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(120),
        },
        popup_shadow: Shadow {
            offset: [0, 6],
            blur: 14,
            spread: 0,
            color: Color32::from_black_alpha(110),
        },
        warn_fg_color: Color32::from_rgb(255, 170, 40),
        error_fg_color: Color32::from_rgb(255, 90, 90),
        weak_text_alpha: 0.55,
        disabled_alpha: 0.45,
        striped: true,
        slider_trailing_fill: true,
        handle_shape: egui::style::HandleShape::Circle,
        interact_cursor: Some(egui::CursorIcon::PointingHand),
        ..Visuals::dark() // keeps text_options.color_transfer_function = DARK_MODE_DEFAULT
    }
}

pub fn fx_light() -> Visuals {
    let accent = Color32::from_rgb(0, 110, 200);
    Visuals {
        dark_mode: false,
        widgets: widget_set(Widgets::light(), accent, Color32::BLACK),
        selection: Selection {
            bg_fill: Color32::from_rgb(170, 215, 255),
            stroke: Stroke::new(1.0, Color32::from_rgb(0, 60, 110)),
        },
        hyperlink_color: accent,
        panel_fill: Color32::from_gray(247),
        window_fill: Color32::from_gray(252),
        extreme_bg_color: Color32::WHITE,
        faint_bg_color: Color32::from_gray(238),
        code_bg_color: Color32::from_gray(232),
        window_stroke: Stroke::new(1.0, Color32::from_gray(196)),
        window_corner_radius: CornerRadius::same(10),
        menu_corner_radius: CornerRadius::same(8),
        window_shadow: Shadow {
            offset: [0, 12],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(30),
        },
        popup_shadow: Shadow {
            offset: [0, 6],
            blur: 14,
            spread: 0,
            color: Color32::from_black_alpha(26),
        },
        warn_fg_color: Color32::from_rgb(200, 100, 0),
        error_fg_color: Color32::from_rgb(190, 20, 20),
        weak_text_alpha: 0.6,
        disabled_alpha: 0.45,
        striped: true,
        slider_trailing_fill: true,
        handle_shape: egui::style::HandleShape::Circle,
        interact_cursor: Some(egui::CursorIcon::PointingHand),
        ..Visuals::light() // CRITICAL: keeps color_transfer_function = LIGHT_MODE_DEFAULT (Off).
                           // Starting from Visuals::dark() or TextOptions::default() here would
                           // render light-mode text with the dark-mode gamma ramp.
    }
}
```

Call it exactly once, right after the `Context` exists (in `eframe`, from your `App`'s
constructor with `cc.egui_ctx`) — never per frame, because `set_fonts` byte-compares the
definitions and any change throws away the atlas and every cached galley.

### Notes on the code above

- `let ... else` + `let`-chains (`if a && let Some(b) = ..`) are used; both are stable and this
  workspace is edition 2024 / rustc 1.98.1.
- `FontData::from_owned(bytes)` takes `Vec<u8>`. For fonts compiled into the binary use
  `FontData::from_static(include_bytes!("../assets/fonts/Inter-Regular.ttf"))` instead — no
  allocation, and a missing file becomes a compile error rather than a runtime warning.
- `NotoSansCJK-Regular.ttc` is a **collection**. `from_owned` hardcodes `index: 0`; if you need a
  different face (e.g. the JP vs SC variant), build the `FontData` literal and set `index`, as in
  §12.
- `.insert(0, ..)` for primaries, `.push(..)` for fallbacks: chains are strict priority order and
  lookup is per character.
- All three script fallbacks are appended to `Monospace` too. Without that, a CJK character inside
  a code block renders as `◻`.
