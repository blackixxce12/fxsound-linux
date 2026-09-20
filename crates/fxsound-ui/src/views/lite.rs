//! The Lite window — 550 × 189, the two pickers and nothing else.
//!
//! Ports `FxLiteView` (`fxsound/Source/GUI/FxLiteView.cpp`, 58 lines — the whole view is a
//! `resized()` that moves two combo boxes and a `paint()` that fills one rounded rectangle). The
//! content is 550 × 112 at `(0, 57)`; the title bar above it is the same
//! [`crate::views::titlebar`] the Pro window uses, with its own `Chrome` because the bar is 508
//! points wide instead of 998.
//!
//! ```text
//! window   (0,   0, 550, 189)   rounded, radius 21, WindowBackground
//! title    (21,  0, 508,  56)   views::titlebar
//! divider  (0,  56, 550,   1)   ControlBackground
//! panel    (20, 79, 510,  90)   rounded, radius 10, DefaultFill α 0.2
//! preset   (40, 99, 225,  50)   output (285, 99, 225, 50)
//! ```
//!
//! Note what is *not* shared with Pro. The panel is `DefaultFill` at 20 % — white in the light
//! palette, black in the dark one — where Pro's is `PanelBackground`, and it is rounded by 10
//! rather than 8 (`FxLiteView.cpp:54` against `FxProView.cpp:114`). The combos keep `FxView`'s own
//! 225 × 50 size because `FxLiteView` never overrides it, which also makes them rounder (`height /
//! 5` = 10) and sets their text in 17 px rather than 14 (`docs/spec/01-window-layout.md` §5.1's
//! shadowing note).
//!
//! ## Two things the original does here that this cannot
//!
//! * **Corner snapping.** `FxMainWindow::showLiteView` teleports the window to within 10 points of
//!   whichever work-area corner the tray icon is in, every single time it is shown
//!   (`FxMainWindow.cpp:266-276`). A Wayland client cannot position itself at all; doing it
//!   properly needs `wlr-layer-shell`, which eframe does not expose
//!   (`docs/spec/01-window-layout.md` §11.1). Placement is the compositor's here.
//! * **The error notification.** `FxView::showErrorNotification` places a 560 × 120 bubble by
//!   subtracting its width from the output combo's, which in a 550 point window puts it at
//!   x = −50 and clips it (§5.1). Nothing is drawn for it yet; when it is, it should be
//!   right-aligned to the combo rather than reproducing that.

use crate::assets::AssetCache;
use crate::layout;
use crate::state::{UiResponse, UiState};
use crate::theme::{FxColor, Palette};
use crate::views::{self, ViewScratch, at, titlebar, window_origin, window_rect};
use egui::{CornerRadius, Pos2, Rect, Ui, pos2, vec2};
use fxsound_core::ViewMode;

/// `cornerSize` of the Lite panel (`FxLiteView.cpp:54`) — ten, not the Pro view's eight.
pub const PANEL_CORNER_RADIUS: f32 = 10.0;

/// Alpha the panel is filled at (`FxLiteView.cpp:54`: `FXCOLOR(DefaultFill).withAlpha(0.2f)`).
pub const PANEL_ALPHA: f32 = 0.2;

/// Points of panel above and below the combos.
///
/// `BACKGROUND_HEIGHT = LIST_HEIGHT + 40` (`FxLiteView.h:41`), i.e. 20 on each side, and the panel
/// starts at content-local y = 22 against the combos' 42 (`FxLiteView.cpp:54`, `:37`).
pub const PANEL_PADDING: f32 = 20.0;

/// The rounded panel behind the two pickers.
///
/// `docs/spec/01-window-layout.md` §8.2 puts it at window `(20, 79, 510, 90)`: content-local
/// `(20, 22)` plus the 57 point content origin, which leaves exactly [`PANEL_PADDING`] above and
/// below the 50 point combos at y = 99. It is derived from [`crate::layout::lite::preset_combo`]
/// here rather than read from [`crate::layout::lite::panel`] because that function currently
/// returns y = 73 — the *Pro* panel's y — which would sit the panel six points high of the combos
/// it is supposed to frame. Deriving it keeps the two from drifting apart whichever way that is
/// settled.
#[must_use]
pub fn panel(origin: Pos2) -> Rect {
    let combos = layout::lite::preset_combo().union(layout::lite::output_combo());
    let reference = layout::lite::panel();
    at(
        origin,
        Rect::from_min_size(
            pos2(reference.left(), combos.top() - PANEL_PADDING),
            vec2(reference.width(), combos.height() + PANEL_PADDING * 2.0),
        ),
    )
}

/// Paint the Lite window and report what the user did.
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    scratch: &mut ViewScratch,
    palette: Palette,
    assets: &mut AssetCache,
) -> UiResponse {
    let origin = window_origin(ui);

    ui.painter().rect_filled(
        window_rect(origin, ViewMode::Lite),
        CornerRadius::same(layout::WINDOW_CORNER_RADIUS as u8),
        palette.window_background(),
    );
    ui.painter().rect_filled(
        panel(origin),
        CornerRadius::same(PANEL_CORNER_RADIUS as u8),
        palette.color_alpha(FxColor::DefaultFill, PANEL_ALPHA),
    );

    let mut response = titlebar::show(ui, state, scratch, palette, assets);

    views::combos(
        ui,
        state,
        palette,
        assets,
        at(origin, layout::lite::preset_combo()),
        at(origin, layout::lite::output_combo()),
        &mut response,
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{PresetEntry, UiAction};
    use crate::theme;
    use crate::widgets::combo;
    use egui::{Event, RawInput};
    use fxsound_core::{AudioDevice, ThemeMode};

    fn state() -> UiState {
        UiState {
            view: ViewMode::Lite,
            presets: vec![
                PresetEntry {
                    name: "Flat".to_owned(),
                    factory: true,
                    modified: false,
                },
                PresetEntry {
                    name: "My Mix".to_owned(),
                    factory: false,
                    modified: false,
                },
            ],
            selected_preset: Some(1),
            devices: vec![AudioDevice {
                id: 7,
                name: "alsa_output.usb-Focusrite".to_owned(),
                description: "Scarlett 2i2 Analogue Stereo".to_owned(),
                is_default: false,
                direction: fxsound_core::DeviceDirection::Output,
                form_factor: "speaker".into(),
            }],
            selected_device: Some(0),
            ..UiState::default()
        }
    }

    fn test_context() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions());
        ctx
    }

    fn frame(
        ctx: &egui::Context,
        state: &UiState,
        scratch: &mut ViewScratch,
        assets: &mut AssetCache,
        events: Vec<Event>,
    ) -> Vec<UiAction> {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                views::window_size(ViewMode::Lite),
            )),
            events,
            ..Default::default()
        };
        let mut actions = Vec::new();
        ctx.run_ui(input, |ui| {
            actions = show(ui, state, scratch, Palette::new(ThemeMode::Dark), assets).actions;
        })
        .drop_without_applying_deltas();
        actions
    }

    #[test]
    fn the_panel_is_the_one_the_spec_tabulates() {
        // docs/spec/01-window-layout.md §8.2: (20, 79, 510, 90).
        let panel = panel(Pos2::ZERO);
        assert!((panel.left() - 20.0).abs() < 1e-4, "{panel:?}");
        assert!((panel.top() - 79.0).abs() < 1e-4, "{panel:?}");
        assert!((panel.width() - 510.0).abs() < 1e-4, "{panel:?}");
        assert!((panel.height() - 90.0).abs() < 1e-4, "{panel:?}");
    }

    #[test]
    fn the_panel_frames_the_combos_with_twenty_points_on_every_side() {
        let panel = panel(Pos2::ZERO);
        let preset = layout::lite::preset_combo();
        let output = layout::lite::output_combo();
        assert!(panel.contains_rect(preset), "{preset:?} escapes {panel:?}");
        assert!(panel.contains_rect(output), "{output:?} escapes {panel:?}");
        assert!((preset.top() - panel.top() - PANEL_PADDING).abs() < 1e-4);
        assert!((panel.bottom() - preset.bottom() - PANEL_PADDING).abs() < 1e-4);
        assert!((preset.left() - panel.left() - PANEL_PADDING).abs() < 1e-4);
        assert!((panel.right() - output.right() - PANEL_PADDING).abs() < 1e-4);
        // …and a 20 point gutter between them, which is where BACKGROUND_WIDTH's third 20 goes.
        assert!((output.left() - preset.right() - PANEL_PADDING).abs() < 1e-4);
    }

    #[test]
    fn the_panel_sits_inside_the_window_with_twenty_points_of_margin() {
        let window = window_rect(Pos2::ZERO, ViewMode::Lite);
        let panel = panel(Pos2::ZERO);
        assert!(window.contains_rect(panel));
        assert!((panel.left() - window.left() - 20.0).abs() < 1e-4);
        assert!((window.right() - panel.right() - 20.0).abs() < 1e-4);
        // 20 points of dead space under the panel, the rounded bottom corners' room.
        assert!((window.bottom() - panel.bottom() - 20.0).abs() < 1e-4);
    }

    #[test]
    fn the_lite_combos_are_rounder_and_set_larger_than_the_pro_ones() {
        // FxLiteView never overrides FxView's 225 x 50, so `height / 5` is 10 rather than 8 and
        // the font stays at 17 instead of dropping to 14 (docs/spec/01-window-layout.md §5.1).
        let lite = layout::lite::preset_combo();
        let pro = layout::pro::preset_combo();
        assert!((combo::corner_radius(lite.height()) - 10.0).abs() < 1e-4);
        assert!((combo::corner_radius(pro.height()) - 8.0).abs() < 1e-4);
        assert!((combo::font_size(lite.height()) - combo::FONT).abs() < 1e-4);
        assert!((lite.width() - 225.0).abs() < 1e-4);
        assert!((lite.height() - 50.0).abs() < 1e-4);
    }

    #[test]
    fn a_quiet_lite_frame_reports_nothing() {
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = state();
        for _ in 0..2 {
            let actions = frame(&ctx, &state, &mut scratch, &mut assets, Vec::new());
            assert!(actions.is_empty(), "an idle window reported {actions:?}");
        }
    }

    #[test]
    fn nothing_the_window_paints_escapes_the_window() {
        // The Lite window is 490 points shorter and 508 narrower than the Pro one, so anything
        // that kept a Pro coordinate by accident would hang off it.
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = state();
        let window = window_rect(Pos2::ZERO, ViewMode::Lite);
        let input = || RawInput {
            screen_rect: Some(window),
            ..Default::default()
        };

        // One warm-up frame so the SVG textures exist, then the frame that is measured.
        ctx.run_ui(input(), |ui| {
            show(
                ui,
                &state,
                &mut scratch,
                Palette::new(ThemeMode::Dark),
                &mut assets,
            );
        })
        .drop_without_applying_deltas();

        let output = ctx.run_ui(input(), |ui| {
            show(
                ui,
                &state,
                &mut scratch,
                Palette::new(ThemeMode::Dark),
                &mut assets,
            );
        });
        let mut painted = Rect::NOTHING;
        for clipped in &output.shapes {
            let bounds = clipped
                .shape
                .visual_bounding_rect()
                .intersect(clipped.clip_rect);
            if bounds.is_positive() {
                painted = painted.union(bounds);
            }
        }
        output.drop_without_applying_deltas();

        assert!(painted.is_positive(), "the window painted nothing at all");
        assert!(
            window.expand(1.0).contains_rect(painted),
            "{painted:?} spills out of {window:?}"
        );
        assert!(
            painted.width() > window.width() - 2.0 && painted.height() > window.height() - 2.0,
            "only {painted:?} of {window:?} was painted"
        );
    }

    #[test]
    fn the_lite_window_still_carries_the_whole_title_bar() {
        // The flip button is the one chrome button whose position differs between the views, so
        // clicking Lite's proves the bar was laid out with `Chrome::LITE`.
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = state();
        let target = crate::layout::Chrome::LITE.flip.rect().center();

        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(target)],
            vec![
                Event::PointerMoved(target),
                Event::PointerButton {
                    pos: target,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            vec![Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        ] {
            actions.extend(frame(&ctx, &state, &mut scratch, &mut assets, events));
        }
        assert_eq!(actions, vec![UiAction::ToggleView]);
    }

    #[test]
    fn the_preset_combo_dies_with_the_power_and_the_output_combo_does_not() {
        // FxLiteView.cpp:57 gates only `preset_list_` — the user must still be able to change
        // outputs with the power off.
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = UiState {
            power: false,
            ..state()
        };

        let preset = layout::lite::preset_combo().center();
        let output = layout::lite::output_combo().center();
        for _ in 0..2 {
            frame(
                &ctx,
                &state,
                &mut scratch,
                &mut assets,
                vec![Event::PointerMoved(preset)],
            );
        }
        // A disabled combo returns no selection and never opens its menu.
        let actions = frame(
            &ctx,
            &state,
            &mut scratch,
            &mut assets,
            vec![
                Event::PointerMoved(preset),
                Event::PointerButton {
                    pos: preset,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                Event::PointerButton {
                    pos: preset,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
        assert!(
            actions.is_empty(),
            "a powered-down preset list reacted: {actions:?}"
        );

        // The output list is still live: its popup opens on a click.
        frame(
            &ctx,
            &state,
            &mut scratch,
            &mut assets,
            vec![Event::PointerMoved(output)],
        );
        frame(
            &ctx,
            &state,
            &mut scratch,
            &mut assets,
            vec![
                Event::PointerMoved(output),
                Event::PointerButton {
                    pos: output,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                Event::PointerButton {
                    pos: output,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
        // `FxComboBox` hangs its menu off `Id::new("fx_combo_box").with(id_salt)`, and
        // `Popup::default_response_id` is that id with "popup" mixed in.
        let popup_id = egui::Id::new("fx_combo_box")
            .with("output_list")
            .with("popup");
        assert!(
            egui::Popup::is_id_open(&ctx, popup_id),
            "the playback-device list should open with the power off"
        );
    }
}
