//! A standing visual check of the port's chrome, palette and controls.
//!
//! Opens a frameless window with the Pro view's real geometry and paints what the foundation
//! layers can already draw: the rounded window, the panel, the title-bar artwork and the five
//! effect sliders. It is not the application — it is how you see, at a glance, whether a change to
//! the palette or the layout did what you meant.
//!
//! ```text
//! cargo run -p fxsound-ui --example preview
//! cargo run -p fxsound-ui --example preview -- --light
//! ```

use eframe::egui;
use fxsound_core::{Effect, ThemeMode};
use fxsound_ui::{
    AssetCache, FxColor, FxImage, Palette,
    layout::{self, Chrome},
    theme,
    widgets::{FxSlider, slider},
};

fn main() -> eframe::Result<()> {
    let light = std::env::args().any(|a| a == "--light");
    let size = layout::pro::WINDOW_SIZE;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([size.x, size.y])
            .with_min_inner_size([size.x, size.y])
            .with_resizable(false)
            .with_decorations(false)
            .with_transparent(true)
            .with_app_id("com.fxsound.FxSound"),
        ..Default::default()
    };

    eframe::run_native(
        "FxSound preview",
        options,
        Box::new(move |cc| {
            let palette = Palette::new(if light {
                ThemeMode::Light
            } else {
                ThemeMode::Dark
            });
            theme::apply(&cc.egui_ctx, palette);
            Ok(Box::new(Preview::new(palette)))
        }),
    )
}

struct Preview {
    palette: Palette,
    assets: AssetCache,
    effects: [f32; Effect::COUNT],
    master_gain: f32,
    frames: u32,
}

impl Preview {
    fn new(palette: Palette) -> Self {
        Self {
            palette,
            assets: AssetCache::new(),
            // Something recognisable rather than all zeros, so the fills are visible.
            effects: [3.0, 5.0, 7.0, 4.0, 8.0],
            master_gain: -4.0,
            frames: 0,
        }
    }

    fn theme_mode(&self) -> ThemeMode {
        if self.palette.is_dark() {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        }
    }

    /// Paint one piece of title-bar artwork at its exact position.
    fn chrome_icon(&mut self, ui: &egui::Ui, button: layout::ChromeButton, image: FxImage) {
        let rect = button.rect();
        let theme = self.theme_mode();
        if let Some(texture) = self.assets.texture(ui.ctx(), image, theme, rect.size()) {
            ui.painter().image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
    }
}

impl eframe::App for Preview {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Fully transparent: the rounded window is painted by us, so the corners must not be
        // filled by the backend.
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let palette = self.palette;
        let window = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), layout::pro::WINDOW_SIZE);

        // The frameless window: a rounded rectangle in the window background colour.
        ui.painter().rect_filled(
            window,
            egui::CornerRadius::same(layout::WINDOW_CORNER_RADIUS as u8),
            palette.window_background(),
        );

        // The divider under the title bar.
        ui.painter().rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(0.0, layout::TITLE_BAR_HEIGHT),
                egui::vec2(window.width(), layout::TITLE_BAR_DIVIDER_HEIGHT),
            ),
            egui::CornerRadius::ZERO,
            palette.color(FxColor::HighlightedFill),
        );

        // The rounded content panel.
        ui.painter().rect_filled(
            layout::pro::panel(),
            egui::CornerRadius::same(layout::PANEL_CORNER_RADIUS as u8),
            palette.panel_background(),
        );

        // Title bar artwork at the original's coordinates.
        let chrome = Chrome::PRO;
        for (button, image) in [
            (chrome.logo, FxImage::DefaultLogo),
            (chrome.menu, FxImage::MenuButton),
            (chrome.power, FxImage::PowerOnButton),
            (chrome.flip, FxImage::FlipButton),
            (chrome.minimize, FxImage::MinimizeWindowButton),
        ] {
            self.chrome_icon(ui, button, image);
        }

        // Placeholders for the regions the view agents are still filling in.
        for (rect, label) in [
            (layout::pro::preset_combo(), "preset"),
            (layout::pro::output_combo(), "output device"),
            (layout::pro::visualizer(), "visualizer"),
            (layout::pro::equalizer(), "equalizer"),
        ] {
            ui.painter().rect_stroke(
                rect,
                egui::CornerRadius::same(layout::PANEL_CORNER_RADIUS as u8),
                egui::Stroke::new(1.0, palette.color_alpha(FxColor::DefaultText, 0.25)),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                label,
                theme::regular(13.0),
                palette.color_alpha(FxColor::DefaultText, 0.5),
            );
        }

        // The five effect sliders, laid out like FxAudioControls.
        let column = layout::pro::audio_controls();
        let row_height = layout::audio_controls::LABEL_HEIGHT
            + layout::audio_controls::SLIDER_HEIGHT
            + layout::audio_controls::ROW_GAP;
        for (index, effect) in Effect::ALL.into_iter().enumerate() {
            let top = column.top() + layout::audio_controls::Y_MARGIN + index as f32 * row_height;

            ui.painter().text(
                egui::pos2(column.left(), top),
                egui::Align2::LEFT_TOP,
                effect.label(),
                theme::semibold(12.0),
                palette.color(FxColor::DefaultText),
            );
            ui.painter().text(
                egui::pos2(column.right(), top),
                egui::Align2::RIGHT_TOP,
                format!("{:.0}", self.effects[effect as usize]),
                theme::semibold(12.0),
                palette.color(FxColor::HighlightedText),
            );

            let slider_rect = egui::Rect::from_min_size(
                egui::pos2(column.left(), top + layout::audio_controls::LABEL_HEIGHT),
                egui::vec2(
                    layout::audio_controls::SLIDER_WIDTH,
                    layout::audio_controls::SLIDER_HEIGHT,
                ),
            );
            FxSlider::new(&mut self.effects[effect as usize], 0.0, 10.0, 1.0).show(
                ui,
                slider_rect,
                palette,
                &mut self.assets,
                effect.key(),
            );
        }

        // One corrected-fill slider next to the faithful ones, so the difference is visible.
        let master_top = column.top() + layout::audio_controls::Y_MARGIN + 5.0 * row_height + 12.0;
        ui.painter().text(
            egui::pos2(column.left(), master_top),
            egui::Align2::LEFT_TOP,
            "Master gain (corrected fill)",
            theme::regular(11.0),
            palette.color(FxColor::HintText),
        );
        FxSlider::new(&mut self.master_gain, -20.0, 20.0, 2.0)
            .fidelity(slider::Fidelity::Corrected)
            .reset_on_secondary_click(true)
            .default_value(0.0)
            .show(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(column.left(), master_top + 14.0),
                    egui::vec2(
                        layout::audio_controls::SLIDER_WIDTH,
                        layout::audio_controls::SLIDER_HEIGHT,
                    ),
                ),
                palette,
                &mut self.assets,
                "master_gain",
            );

        self.frames += 1;
        // Exit on its own so a screenshot run cannot leave a window behind.
        if std::env::args().any(|a| a == "--exit-after-paint") && self.frames > 120 {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
