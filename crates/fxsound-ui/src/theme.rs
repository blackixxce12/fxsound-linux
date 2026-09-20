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

        v.window_stroke = Stroke::new(1.0, self.color(FxColor::Outline));
        v.window_corner_radius = CornerRadius::same(crate::layout::WINDOW_CORNER_RADIUS as u8);
        v.menu_corner_radius = CornerRadius::same(8);

        v.selection.bg_fill = self.color_alpha(FxColor::SliderHighlight, 0.35);
        v.selection.stroke = Stroke::new(1.0, highlighted_text);

        let widgets = &mut v.widgets;
        widgets.noninteractive.bg_fill = self.window_background();
        widgets.noninteractive.weak_bg_fill = self.window_background();
        widgets.noninteractive.bg_stroke = Stroke::new(1.0, self.color(FxColor::Outline));
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
