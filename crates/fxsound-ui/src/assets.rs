//! The embedded artwork and the SVG rasteriser that turns it into egui textures.
//!
//! The original registers 32 named images per theme and swaps the whole table when the theme
//! changes (`fxsound/Source/GUI/FxTheme.cpp:31-56`). The names and their order are transcribed from
//! `FxTheme.h`'s `FxImage` enum so the two tables can be diffed side by side.
//!
//! Every file is embedded in the binary. FxSound ships its artwork as SVG, so rasterising at the
//! exact size a control needs — including the compositor's fractional scale — costs nothing extra
//! and looks correct at any scale, which a bundled PNG would not.

use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use fxsound_core::ThemeMode;
use std::collections::HashMap;

/// The named images, in `FxTheme.h`'s `FxImage` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FxImage {
    DefaultLogo,
    HighlightedLogo,
    IconLogo,
    PowerOnButton,
    PowerOffButton,
    DonateButton,
    DonateButtonHover,
    MenuButton,
    MenuButtonHover,
    MinimizeButton,
    MinimizeButtonHover,
    MaximizeButton,
    MaximizeButtonHover,
    MinimizeWindowButton,
    MinimizeWindowButtonHover,
    FlipButton,
    FlipButtonHover,
    RestoreDefaultsButton,
    RestoreDefaultsButtonHover,
    RemoveButton,
    ArrowNext,
    ArrowNextBW,
    ArrowPrev,
    ArrowPrevBW,
    ArrowUpSelected,
    ArrowUp,
    ArrowDownSelected,
    ArrowDown,
    DropDownArrow,
    DropDownArrowHover,
    SliderThumb,
    SliderThumbBW,
}

/// `FxImage::NumImages`.
pub const NUM_IMAGES: usize = 32;

macro_rules! svg {
    ($name:literal) => {
        include_bytes!(concat!("../../../assets/images/", $name))
    };
}

/// `FxTheme::theme_images_[FxThemeMode::Dark]`.
const DARK_IMAGES: [&[u8]; NUM_IMAGES] = [
    svg!("logo-white.svg"),
    svg!("logo-red.svg"),
    svg!("FxSound White Bars.svg"),
    svg!("power_on.svg"),
    svg!("power_off.svg"),
    svg!("donate.svg"),
    svg!("donate_hover.svg"),
    svg!("menu.svg"),
    svg!("menu_hover.svg"),
    svg!("minimize.svg"),
    svg!("minimize_hover.svg"),
    svg!("maximize.svg"),
    svg!("maximize_hover.svg"),
    svg!("min_window.svg"),
    svg!("min_window_hover.svg"),
    svg!("flip_white.svg"),
    svg!("flip.svg"),
    svg!("restore_defaults_white.svg"),
    svg!("restore_defaults.svg"),
    svg!("remove.svg"),
    svg!("arrow_next.svg"),
    svg!("arrow_next_bw.svg"),
    svg!("arrow_prev.svg"),
    svg!("arrow_prev_bw.svg"),
    svg!("arrow_up.svg"),
    svg!("arrow_up_white.svg"),
    svg!("arrow_down.svg"),
    svg!("arrow_down_white.svg"),
    svg!("dropdown_arrow_bw.svg"),
    svg!("dropdown_arrow_hover.svg"),
    svg!("Slider_Thumb.svg"),
    svg!("Slider_Thumb_bw.svg"),
];

/// `FxTheme::theme_images_[FxThemeMode::Light]`.
const LIGHT_IMAGES: [&[u8]; NUM_IMAGES] = [
    svg!("logo-black.svg"),
    svg!("logo-blue.svg"),
    svg!("FxSound Black Bars.svg"),
    svg!("power_on_blue.svg"),
    svg!("power_off_black.svg"),
    svg!("donate_blue.svg"),
    svg!("donate_hover_blue.svg"),
    svg!("menu_black.svg"),
    svg!("menu_hover_blue.svg"),
    svg!("minimize_black.svg"),
    svg!("minimize_hover_blue.svg"),
    svg!("maximize_black.svg"),
    svg!("maximize_hover_blue.svg"),
    svg!("min_window_black.svg"),
    svg!("min_window_hover_blue.svg"),
    svg!("flip_black.svg"),
    svg!("flip_blue.svg"),
    svg!("restore_defaults_black.svg"),
    svg!("restore_defaults_blue.svg"),
    svg!("remove.svg"),
    svg!("arrow_next_blue.svg"),
    svg!("arrow_next_bw.svg"),
    svg!("arrow_prev_blue.svg"),
    svg!("arrow_prev_bw.svg"),
    svg!("arrow_up_blue.svg"),
    svg!("arrow_up_black.svg"),
    svg!("arrow_down_blue.svg"),
    svg!("arrow_down_black.svg"),
    svg!("dropdown_arrow_bw.svg"),
    svg!("dropdown_arrow_hover_blue.svg"),
    svg!("Slider_Thumb_blue.svg"),
    svg!("Slider_Thumb_bw.svg"),
];

/// The raw SVG bytes for one image in one theme.
#[must_use]
pub fn svg_bytes(image: FxImage, theme: ThemeMode) -> &'static [u8] {
    let index = image as usize;
    match theme {
        ThemeMode::Dark => DARK_IMAGES[index],
        ThemeMode::Light => LIGHT_IMAGES[index],
    }
}

/// Rasterise an SVG to an egui image at an exact pixel size.
///
/// `width_px` and `height_px` are *physical* pixels: multiply logical points by
/// `ctx.pixels_per_point()` before calling, so the result is sharp under fractional scaling.
///
/// Returns `None` when the SVG cannot be parsed or the size is degenerate; callers draw nothing
/// rather than panicking, because a missing icon must never take the audio path down with it.
#[must_use]
pub fn rasterise(svg: &[u8], width_px: u32, height_px: u32) -> Option<ColorImage> {
    if width_px == 0 || height_px == 0 {
        return None;
    }
    let options = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg, &options).ok()?;

    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        return None;
    }

    let mut pixmap = tiny_skia::Pixmap::new(width_px, height_px)?;
    let transform = tiny_skia::Transform::from_scale(
        width_px as f32 / size.width(),
        height_px as f32 / size.height(),
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny-skia works premultiplied and hands back straight-alpha RGBA bytes, which is exactly
    // what `from_rgba_unmultiplied` wants.
    let rgba = pixmap.take_demultiplied();
    Some(ColorImage::from_rgba_unmultiplied(
        [width_px as usize, height_px as usize],
        &rgba,
    ))
}

/// Recolour an image, keeping its alpha.
///
/// The original ships one SVG per colour variant, which covers the themed artwork. A tint is still
/// useful for states the artwork does not have — a disabled control, for instance.
#[must_use]
pub fn tinted(image: &ColorImage, colour: Color32) -> ColorImage {
    let pixels = image
        .pixels
        .iter()
        .map(|p| {
            Color32::from_rgba_premultiplied(
                (u16::from(colour.r()) * u16::from(p.a()) / 255) as u8,
                (u16::from(colour.g()) * u16::from(p.a()) / 255) as u8,
                (u16::from(colour.b()) * u16::from(p.a()) / 255) as u8,
                p.a(),
            )
        })
        .collect();
    ColorImage::new(image.size, pixels)
}

/// Key for one rasterised variant: the image, the theme and the physical size it was drawn at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    image: FxImage,
    theme: ThemeMode,
    width_px: u32,
    height_px: u32,
}

/// Lazily rasterises artwork and holds the resulting GPU textures.
///
/// One instance lives in the application state. Textures are uploaded once per (image, theme,
/// size) and reused; `load_texture` must never be called per frame.
#[derive(Default)]
pub struct AssetCache {
    textures: HashMap<CacheKey, TextureHandle>,
}

impl std::fmt::Debug for AssetCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssetCache")
            .field("textures", &self.textures.len())
            .finish()
    }
}

impl AssetCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A texture for `image` sized to fit `size_points` at the context's current scale.
    ///
    /// Returns `None` only if the SVG fails to parse, which would be a build-time mistake.
    pub fn texture(
        &mut self,
        ctx: &Context,
        image: FxImage,
        theme: ThemeMode,
        size_points: egui::Vec2,
    ) -> Option<&TextureHandle> {
        let scale = ctx.pixels_per_point().max(0.1);
        let width_px = (size_points.x * scale).round().max(1.0) as u32;
        let height_px = (size_points.y * scale).round().max(1.0) as u32;
        let key = CacheKey {
            image,
            theme,
            width_px,
            height_px,
        };

        if let std::collections::hash_map::Entry::Vacant(slot) = self.textures.entry(key) {
            let colour_image = rasterise(svg_bytes(image, theme), width_px, height_px)?;
            let name = format!("{image:?}-{theme:?}-{width_px}x{height_px}");
            let handle = ctx.load_texture(name, colour_image, TextureOptions::LINEAR);
            slot.insert(handle);
        }
        self.textures.get(&key)
    }

    /// Drop every cached texture, e.g. after a theme change that will never use them again.
    pub fn clear(&mut self) {
        self.textures.clear();
    }

    /// How many textures are currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.textures.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.textures.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [FxImage; NUM_IMAGES] = [
        FxImage::DefaultLogo,
        FxImage::HighlightedLogo,
        FxImage::IconLogo,
        FxImage::PowerOnButton,
        FxImage::PowerOffButton,
        FxImage::DonateButton,
        FxImage::DonateButtonHover,
        FxImage::MenuButton,
        FxImage::MenuButtonHover,
        FxImage::MinimizeButton,
        FxImage::MinimizeButtonHover,
        FxImage::MaximizeButton,
        FxImage::MaximizeButtonHover,
        FxImage::MinimizeWindowButton,
        FxImage::MinimizeWindowButtonHover,
        FxImage::FlipButton,
        FxImage::FlipButtonHover,
        FxImage::RestoreDefaultsButton,
        FxImage::RestoreDefaultsButtonHover,
        FxImage::RemoveButton,
        FxImage::ArrowNext,
        FxImage::ArrowNextBW,
        FxImage::ArrowPrev,
        FxImage::ArrowPrevBW,
        FxImage::ArrowUpSelected,
        FxImage::ArrowUp,
        FxImage::ArrowDownSelected,
        FxImage::ArrowDown,
        FxImage::DropDownArrow,
        FxImage::DropDownArrowHover,
        FxImage::SliderThumb,
        FxImage::SliderThumbBW,
    ];

    #[test]
    fn every_image_exists_in_both_themes() {
        for (index, image) in ALL.into_iter().enumerate() {
            assert_eq!(image as usize, index, "{image:?} is out of enum order");
            for theme in [ThemeMode::Dark, ThemeMode::Light] {
                let bytes = svg_bytes(image, theme);
                assert!(!bytes.is_empty(), "{image:?} {theme:?} is empty");
            }
        }
    }

    #[test]
    fn every_image_rasterises() {
        for image in ALL {
            for theme in [ThemeMode::Dark, ThemeMode::Light] {
                let raster = rasterise(svg_bytes(image, theme), 32, 32);
                let raster = raster.unwrap_or_else(|| panic!("{image:?} {theme:?} failed to render"));
                assert_eq!(raster.size, [32, 32]);
                assert_eq!(raster.pixels.len(), 32 * 32);
            }
        }
    }

    #[test]
    fn a_rasterised_icon_is_not_blank() {
        // The power button is a solid glyph; if it comes out fully transparent the renderer is
        // silently doing nothing.
        let raster = rasterise(svg_bytes(FxImage::PowerOnButton, ThemeMode::Dark), 48, 48)
            .expect("render");
        assert!(
            raster.pixels.iter().any(|p| p.a() > 0),
            "the rendered icon was entirely transparent"
        );
    }

    #[test]
    fn the_two_theme_tables_differ_where_they_should() {
        // Only RemoveButton, ArrowNextBW, ArrowPrevBW, DropDownArrow and SliderThumbBW are shared.
        let shared = [
            FxImage::RemoveButton,
            FxImage::ArrowNextBW,
            FxImage::ArrowPrevBW,
            FxImage::DropDownArrow,
            FxImage::SliderThumbBW,
        ];
        for image in ALL {
            let dark = svg_bytes(image, ThemeMode::Dark);
            let light = svg_bytes(image, ThemeMode::Light);
            if shared.contains(&image) {
                assert_eq!(dark.as_ptr(), light.as_ptr(), "{image:?} should be shared");
            } else {
                assert_ne!(dark.as_ptr(), light.as_ptr(), "{image:?} should differ per theme");
            }
        }
    }

    #[test]
    fn a_degenerate_size_is_refused_rather_than_panicking() {
        assert!(rasterise(svg_bytes(FxImage::MenuButton, ThemeMode::Dark), 0, 16).is_none());
        assert!(rasterise(svg_bytes(FxImage::MenuButton, ThemeMode::Dark), 16, 0).is_none());
    }

    #[test]
    fn invalid_svg_data_is_refused_rather_than_panicking() {
        assert!(rasterise(b"this is not an svg", 16, 16).is_none());
    }

    #[test]
    fn tinting_keeps_the_alpha_channel() {
        let raster = rasterise(svg_bytes(FxImage::PowerOnButton, ThemeMode::Dark), 24, 24)
            .expect("render");
        let tint = Color32::from_rgb(255, 0, 0);
        let tinted = tinted(&raster, tint);
        assert_eq!(tinted.size, raster.size);
        for (before, after) in raster.pixels.iter().zip(&tinted.pixels) {
            assert_eq!(before.a(), after.a(), "alpha must survive a tint");
        }
        assert!(
            tinted.pixels.iter().any(|p| p.r() > 0 && p.g() == 0 && p.b() == 0),
            "the tint colour never appeared"
        );
    }
}
