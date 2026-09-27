//! The FxSound palette, fonts and the egui `Visuals` derived from them.
//!
//! The two palettes are transcribed verbatim from `FxTheme::theme_colors_`
//! (`fxsound/Source/GUI/FxTheme.cpp:22-28`). The original stores them as 24-bit RGB and applies an
//! alpha at each use site, so [`Palette::color`] returns an opaque colour and the callers that need
//! transparency say so explicitly — exactly as the C++ does.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, Visuals};
use fxsound_core::ThemeMode;
use std::sync::Arc;

/// Every named colour in the original theme, in `FxColor` order.
///
/// The order matters: it indexes [`DARK`] and [`LIGHT`], which are transcribed as flat arrays so
/// they can be diffed line-by-line against `FxTheme.cpp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum FxColor {
    WindowBackground = 0,
    WidgetBackground = 1,
    MenuBackground = 2,
    Outline = 3,
    DefaultText = 4,
    DefaultFill = 5,
    HighlightedText = 6,
    HighlightedFill = 7,
    MenuText = 8,
    ComboBoxBackground = 9,
    TextButtonBackground = 10,
    ImageButton = 11,
    HintText = 12,
    ValidTextBorder = 13,
    InvalidTextBorder = 14,
    ControlBackground = 15,
    SliderTrack = 16,
    SliderHighlight = 17,
    GraphHigh = 18,
    GraphLow = 19,
    EqStart = 20,
    EqEnd = 21,
    VerticalSliderLow = 22,
    MenuHighlightBackground = 23,
    PanelBackground = 24,
    RowOutline = 25,
    SelectedRowOutline = 26,
}

/// Number of entries in each palette (`FxColor::NumColors`).
pub const NUM_COLORS: usize = 27;

/// `FxTheme::theme_colors_[FxThemeMode::Dark]`.
pub const DARK: [u32; NUM_COLORS] = [
    0x18_1818, 0x18_1818, 0x38_3838, 0x2b_2b2b, 0xb1_b1b1, 0x00_0000, 0xff_ffff, 0x0c_0c0c,
    0xff_ffff, 0x00_0000, 0xd5_1535, 0xe6_3462, 0x7f_7f7f, 0x00_9cdd, 0xd5_1535, 0x0f_0f0f,
    0xe3_3250, 0xf7_546f, 0xd5_1535, 0xfe_566a, 0xef_4b65, 0x74_2834, 0xf3_f3f3, 0x41_4141,
    0x00_0000, 0xb1_b1b1, 0xe6_3462,
];

/// `FxTheme::theme_colors_[FxThemeMode::Light]`.
pub const LIGHT: [u32; NUM_COLORS] = [
    0xf5_f5f5, 0xf5_f5f5, 0xc7_c7c7, 0xfa_fafa, 0x4e_4e4e, 0xff_ffff, 0x00_0000, 0xe0_e0e0,
    0x00_0000, 0xd7_d7d7, 0x1a_c1ff, 0x23_b6eb, 0x7f_7f7f, 0x00_9cdd, 0xd5_1535, 0xe0_e0e0,
    0x0a_4d66, 0x53_ccff, 0x1a_c1ff, 0x72_d8ff, 0x33_c8ff, 0x06_3244, 0x1c_1c1c, 0xb9_b9b9,
    0xc0_c0c0, 0x4e_4e4e, 0x23_b6eb,
];

/// The lightest grey [`Palette::greyed`] gives in the light palette: `#767676`, 3.4:1 on the light
/// `ControlBackground` (`#e0e0e0`) the visualizer and the equalizer are drawn on.
pub const LIGHT_GREY_LIMIT: u8 = 0x76;

/// WCAG 2's contrast ratio between two opaque colours, 1 to 21.
#[must_use]
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    fn channel(value: u8) -> f32 {
        let c = f32::from(value) / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    let luminance =
        |c: Color32| 0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b());
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The active palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    mode: ThemeMode,
}

impl Palette {
    #[must_use]
    pub const fn new(mode: ThemeMode) -> Self {
        Self { mode }
    }

    #[must_use]
    pub const fn mode(self) -> ThemeMode {
        self.mode
    }

    #[must_use]
    pub const fn is_dark(self) -> bool {
        matches!(self.mode, ThemeMode::Dark)
    }

    /// An opaque colour from the active palette.
    #[must_use]
    pub const fn color(self, id: FxColor) -> Color32 {
        let table = match self.mode {
            ThemeMode::Dark => &DARK,
            ThemeMode::Light => &LIGHT,
        };
        let rgb = table[id as usize];
        Color32::from_rgb(
            ((rgb >> 16) & 0xff) as u8,
            ((rgb >> 8) & 0xff) as u8,
            (rgb & 0xff) as u8,
        )
    }

    /// A colour from the active palette at a given alpha, matching the original's
    /// `Colour(...).withAlpha(a)` idiom.
    #[must_use]
    pub fn color_alpha(self, id: FxColor, alpha: f32) -> Color32 {
        let c = self.color(id);
        Color32::from_rgba_unmultiplied(
            c.r(),
            c.g(),
            c.b(),
            (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
        )
    }

    /// The window's background, used for the root clear colour too.
    #[must_use]
    pub const fn window_background(self) -> Color32 {
        self.color(FxColor::WindowBackground)
    }

    /// The rounded content panel behind the controls (`PanelBackground` at α 0.2).
    #[must_use]
    pub fn panel_background(self) -> Color32 {
        self.color_alpha(FxColor::PanelBackground, 0.2)
    }

    /// A rule between two areas and the edge of a menu: `Outline`, except in the light palette.
    ///
    /// The light `Outline` is `#fafafa`, 1.04:1 on the `#f5f5f5` window, so the Settings rule and
    /// the hamburger menu's edge the original draws in it are not there at all (0.4.0 audit #25).
    /// The light palette's own `#c0c0c0` — its `PanelBackground` — is used instead; the palette
    /// tables are left as the original's.
    #[must_use]
    pub const fn divider(self) -> Color32 {
        match self.mode {
            ThemeMode::Dark => self.color(FxColor::Outline),
            ThemeMode::Light => Color32::from_rgb(0xc0, 0xc0, 0xc0),
        }
    }

    /// `colour` greyed out, as a graph shows it while what it draws is switched off: the
    /// visualizer with the power off and the equalizer's curve while it is bypassed.
    ///
    /// The original's `Colour::withSaturation(0.0f)` round-trips through HSB and keeps the
    /// brightness, `max(r, g, b)` (`FxVisualizer.cpp:177-199`), which the dark palette keeps. In
    /// the light palette that turns the light blues white, `#1ac1ff` and `#72d8ff` into `#ffffff`
    /// on a `#e0e0e0` panel, 1.3:1, and the graph is gone — "a real legibility bug", which 0.3.0
    /// reproduced (0.4.0 audit #24). There the grey is the colour's luma instead, and no lighter
    /// than [`LIGHT_GREY_LIMIT`], which still reads at 3:1 on the panel. Alpha is kept.
    #[must_use]
    pub fn greyed(self, colour: Color32) -> Color32 {
        let [r, g, b, a] = colour.to_srgba_unmultiplied();
        let grey = match self.mode {
            ThemeMode::Dark => r.max(g).max(b),
            ThemeMode::Light => {
                let luma = 0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b);
                (luma.round() as u8).min(LIGHT_GREY_LIMIT)
            }
        };
        Color32::from_rgba_unmultiplied(grey, grey, grey, a)
    }

    /// egui `Visuals` that make stock widgets look like the FxSound ones.
    ///
    /// Custom-painted widgets (sliders, the EQ, the visualizer, the title bar) do not read these;
    /// they take colours from the palette directly. This exists so that anything drawn with a
    /// stock egui widget — menus, text edits, scroll bars, tooltips — still matches.
    #[must_use]
    pub fn visuals(self) -> Visuals {
        let mut v = if self.is_dark() {
            Visuals::dark()
        } else {
            Visuals::light()
        };

        let text = self.color(FxColor::DefaultText);
        let highlighted_text = self.color(FxColor::HighlightedText);
        let fill = self.color(FxColor::DefaultFill);
        let accent = self.color(FxColor::TextButtonBackground);

        v.override_text_color = Some(text);
        v.panel_fill = self.window_background();
        v.window_fill = self.window_background();
        v.extreme_bg_color = fill;
        v.faint_bg_color = self.color(FxColor::ControlBackground);
        v.code_bg_color = self.color(FxColor::ControlBackground);
        v.hyperlink_color = highlighted_text;
        v.warn_fg_color = self.color(FxColor::InvalidTextBorder);
        v.error_fg_color = self.color(FxColor::InvalidTextBorder);

        v.window_stroke = Stroke::new(1.0, self.divider());
        v.window_corner_radius = CornerRadius::same(crate::layout::WINDOW_CORNER_RADIUS as u8);
        v.menu_corner_radius = CornerRadius::same(8);

        v.selection.bg_fill = self.color_alpha(FxColor::SliderHighlight, 0.35);
        v.selection.stroke = Stroke::new(1.0, highlighted_text);

        let widgets = &mut v.widgets;
        widgets.noninteractive.bg_fill = self.window_background();
        widgets.noninteractive.weak_bg_fill = self.window_background();
        widgets.noninteractive.bg_stroke = Stroke::new(1.0, self.divider());
        widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);

        widgets.inactive.bg_fill = self.color(FxColor::ComboBoxBackground);
        widgets.inactive.weak_bg_fill = self.color(FxColor::ComboBoxBackground);
        widgets.inactive.bg_stroke = Stroke::new(1.0, self.color(FxColor::ComboBoxBackground));
        widgets.inactive.fg_stroke = Stroke::new(1.0, text);

        widgets.hovered.bg_fill = self.color(FxColor::MenuHighlightBackground);
        widgets.hovered.weak_bg_fill = self.color(FxColor::MenuHighlightBackground);
        widgets.hovered.bg_stroke = Stroke::new(1.0, self.color(FxColor::ImageButton));
        widgets.hovered.fg_stroke = Stroke::new(1.0, highlighted_text);

        widgets.active.bg_fill = accent;
        widgets.active.weak_bg_fill = accent;
        widgets.active.bg_stroke = Stroke::new(1.0, accent);
        widgets.active.fg_stroke = Stroke::new(1.0, highlighted_text);

        widgets.open.bg_fill = self.color(FxColor::ComboBoxBackground);
        widgets.open.weak_bg_fill = self.color(FxColor::ComboBoxBackground);
        widgets.open.bg_stroke = Stroke::new(1.0, self.color(FxColor::ValidTextBorder));
        widgets.open.fg_stroke = Stroke::new(1.0, text);

        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(8);
        }

        v
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::new(ThemeMode::Dark)
    }
}

/// Font family names registered with egui.
pub mod fonts {
    /// Gilroy Regular — body text.
    pub const REGULAR: &str = "Gilroy-Regular";
    /// Gilroy Semibold — labels and values.
    pub const SEMIBOLD: &str = "Gilroy-Semibold";
    /// Gilroy Bold — headings and the preset name.
    pub const BOLD: &str = "Gilroy-Bold";
}

/// The Gilroy faces, embedded so the binary does not depend on a font being installed.
const GILROY_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Gilroy-Regular.ttf");
const GILROY_SEMIBOLD: &[u8] = include_bytes!("../../../assets/fonts/Gilroy-Semibold.ttf");
const GILROY_BOLD: &[u8] = include_bytes!("../../../assets/fonts/Gilroy-Bold.ttf");

/// Script fallbacks, so a Chinese, Korean, Arabic or Thai UI language still renders.
const NOTO_SC: &[u8] = include_bytes!("../../../assets/fonts/NotoSansSC-Regular.otf");
const NOTO_KR: &[u8] = include_bytes!("../../../assets/fonts/NotoSansKR-Regular.otf");
const NOTO_ARABIC: &[u8] = include_bytes!("../../../assets/fonts/NotoSansArabic-Regular.ttf");
const NOTO_THAI: &[u8] = include_bytes!("../../../assets/fonts/NotoSansThai-Regular.ttf");

/// Register Gilroy plus the CJK/Arabic/Thai fallbacks.
///
/// `Proportional` resolves to Gilroy Regular and falls back through the Noto faces, so a mixed
/// string renders without tofu. The semibold and bold faces are their own families because the
/// original picks a face per control rather than deriving a weight.
#[must_use]
pub fn font_definitions() -> FontDefinitions {
    let mut defs = FontDefinitions::default();

    let mut insert = |name: &str, bytes: &'static [u8]| {
        defs.font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    };
    insert(fonts::REGULAR, GILROY_REGULAR);
    insert(fonts::SEMIBOLD, GILROY_SEMIBOLD);
    insert(fonts::BOLD, GILROY_BOLD);
    insert("NotoSansSC", NOTO_SC);
    insert("NotoSansKR", NOTO_KR);
    insert("NotoSansArabic", NOTO_ARABIC);
    insert("NotoSansThai", NOTO_THAI);

    let fallbacks = [
        "NotoSansSC".to_owned(),
        "NotoSansKR".to_owned(),
        "NotoSansArabic".to_owned(),
        "NotoSansThai".to_owned(),
    ];

    // Proportional: Gilroy first, then the scripts Gilroy has no glyphs for, then egui's own
    // default face so symbols and emoji still resolve.
    let proportional = defs.families.entry(FontFamily::Proportional).or_default();
    let stock = std::mem::take(proportional);
    proportional.push(fonts::REGULAR.to_owned());
    proportional.extend(fallbacks.iter().cloned());
    proportional.extend(stock);

    for (family, primary) in [
        (fonts::SEMIBOLD, fonts::SEMIBOLD),
        (fonts::BOLD, fonts::BOLD),
    ] {
        let chain = std::iter::once(primary.to_owned())
            .chain(fallbacks.iter().cloned())
            .collect();
        defs.families.insert(FontFamily::Name(family.into()), chain);
    }

    defs
}

/// A [`FontId`] in the Gilroy regular family.
#[must_use]
pub fn regular(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

/// A [`FontId`] in the Gilroy semibold family.
#[must_use]
pub fn semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::SEMIBOLD.into()))
}

/// A [`FontId`] in the Gilroy bold family.
#[must_use]
pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::BOLD.into()))
}

/// Install fonts and visuals on a context. Call once at startup and again on a theme change.
pub fn apply(ctx: &egui::Context, palette: Palette) {
    ctx.set_fonts(font_definitions());
    ctx.set_visuals(palette.visuals());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_palettes_are_complete() {
        assert_eq!(DARK.len(), NUM_COLORS);
        assert_eq!(LIGHT.len(), NUM_COLORS);
    }

    #[test]
    fn transcription_spot_checks_match_fxtheme_cpp() {
        let dark = Palette::new(ThemeMode::Dark);
        let light = Palette::new(ThemeMode::Light);

        assert_eq!(
            dark.color(FxColor::WindowBackground),
            Color32::from_rgb(0x18, 0x18, 0x18)
        );
        assert_eq!(
            light.color(FxColor::WindowBackground),
            Color32::from_rgb(0xf5, 0xf5, 0xf5)
        );
        // The FxSound red and its light-theme blue counterpart.
        assert_eq!(
            dark.color(FxColor::TextButtonBackground),
            Color32::from_rgb(0xd5, 0x15, 0x35)
        );
        assert_eq!(
            light.color(FxColor::TextButtonBackground),
            Color32::from_rgb(0x1a, 0xc1, 0xff)
        );
        // InvalidTextBorder is the one colour that is identical in both themes.
        assert_eq!(
            dark.color(FxColor::InvalidTextBorder),
            light.color(FxColor::InvalidTextBorder)
        );
        assert_eq!(
            dark.color(FxColor::HintText),
            light.color(FxColor::HintText)
        );
    }

    #[test]
    fn window_and_widget_background_are_identical_per_theme() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let p = Palette::new(mode);
            assert_eq!(
                p.color(FxColor::WindowBackground),
                p.color(FxColor::WidgetBackground)
            );
        }
    }

    #[test]
    fn the_light_divider_can_be_seen_on_the_window_and_on_a_menu_and_the_dark_one_is_the_outline() {
        // 0.4.0 audit #25: the light `Outline`, #fafafa, is 1.04:1 on the #f5f5f5 window.
        let light = Palette::new(ThemeMode::Light);
        let outline = light.color(FxColor::Outline);
        assert!(contrast_ratio(outline, light.window_background()) < 1.1);
        for behind in [light.window_background(), light.color(FxColor::DefaultFill)] {
            let ratio = contrast_ratio(light.divider(), behind);
            assert!(ratio > 1.5, "{ratio} on {behind:?}");
        }
        let dark = Palette::new(ThemeMode::Dark);
        assert_eq!(dark.divider(), dark.color(FxColor::Outline));
        // The palette table itself stays the original's.
        assert_eq!(LIGHT[FxColor::Outline as usize], 0xfa_fafa);
    }

    /// `docs/spec/04-equalizer-visualizer.md` §A8's precomputed greys, dark column:
    /// `Colour::withSaturation(0)` keeps HSB brightness, the channel maximum, not a mid grey.
    #[test]
    fn the_dark_palette_greys_as_the_original_does_keeping_the_brightest_channel() {
        let dark = Palette::new(ThemeMode::Dark);
        let spec_table = [
            (FxColor::SliderTrack, 0xe3),
            (FxColor::GraphHigh, 0xd5),
            (FxColor::GraphLow, 0xfe),
            (FxColor::EqStart, 0xef),
            (FxColor::EqEnd, 0x74),
            (FxColor::SliderHighlight, 0xf7),
        ];
        for (id, expected) in spec_table {
            assert_eq!(
                dark.greyed(dark.color(id)),
                Color32::from_rgb(expected, expected, expected),
                "{id:?}"
            );
        }
    }

    #[test]
    fn the_dark_palette_keeps_the_alpha_of_what_it_greys() {
        let dark = Palette::new(ThemeMode::Dark);
        let translucent = dark.color_alpha(FxColor::EqEnd, 0x55 as f32 / 255.0);
        let [r, g, b, a] = dark.greyed(translucent).to_srgba_unmultiplied();
        assert_eq!(a, 0x55);
        assert!(r == g && g == b, "{:?}", [r, g, b]);
        // The grey is `EqEnd`'s own brightest channel, 0x74, and not the premultiplied one, 0x26;
        // storing it premultiplied at a third alpha costs at most one step of rounding.
        assert!(r.abs_diff(0x74) <= 1, "{r:#x}");
    }

    #[test]
    fn the_light_palette_greys_to_luma_no_lighter_than_three_to_one_on_its_panels() {
        // 0.4.0 audit #24: `withSaturation(0)` turned both light graph colours white.
        let light = Palette::new(ThemeMode::Light);
        let panel = light.color(FxColor::ControlBackground);
        for id in [
            FxColor::GraphHigh,
            FxColor::GraphLow,
            FxColor::EqStart,
            FxColor::EqEnd,
            FxColor::SliderTrack,
            FxColor::VerticalSliderLow,
        ] {
            let grey = light.greyed(light.color(id));
            assert!(
                grey.r() == grey.g() && grey.g() == grey.b(),
                "{id:?}: {grey:?}"
            );
            assert!(grey.r() <= LIGHT_GREY_LIMIT, "{id:?}: {grey:?}");
            let ratio = contrast_ratio(grey, panel);
            assert!(
                ratio >= 3.0,
                "{id:?}: {grey:?} is {ratio:.2}:1 on {panel:?}"
            );
        }
        // A dark colour keeps its own luma rather than being lifted to the limit.
        assert_eq!(
            light.greyed(light.color(FxColor::SliderTrack)),
            Color32::from_rgb(0x41, 0x41, 0x41)
        );
        assert_eq!(
            light
                .greyed(Color32::from_rgba_unmultiplied(0x1a, 0xc1, 0xff, 191))
                .a(),
            191
        );
    }

    #[test]
    fn the_contrast_ratio_is_wcags() {
        assert!((contrast_ratio(Color32::BLACK, Color32::WHITE) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(Color32::WHITE, Color32::WHITE) - 1.0).abs() < 1e-6);
        // #767676 on white is the classic 4.54:1.
        let grey = Color32::from_rgb(0x76, 0x76, 0x76);
        assert!((contrast_ratio(grey, Color32::WHITE) - 4.54).abs() < 0.01);
    }

    #[test]
    fn alpha_helper_preserves_rgb() {
        let p = Palette::default();
        let base = p.color(FxColor::PanelBackground);
        let faded = p.color_alpha(FxColor::PanelBackground, 0.2);
        assert_eq!(
            (faded.r(), faded.g(), faded.b()),
            (base.r(), base.g(), base.b())
        );
        assert_eq!(faded.a(), 51);
    }
}
