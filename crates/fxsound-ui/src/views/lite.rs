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
//!   x = −50 and clips it (§5.1). A notice is drawn here instead as one line under the two
//!   combos, inside the panel's bottom padding ([`crate::layout::lite::notification`]), and a
//!   click on it takes it down.
//!
//! ## Two lanes in one list
//!
//! The Pro view has a device list per lane; this window keeps its one list and holds both lanes in
//! it: `Output` and `Input` titled as before, each run starting with an `Off` row that detaches
//! that lane. Picking a device makes its lane the one the preset list addresses, and the closed box
//! shows that lane's device — or `Off`, dimmed, when it has none.

use crate::assets::AssetCache;
use crate::layout;
use crate::state::{UiAction, UiResponse, UiState};
use crate::theme::{FxColor, Palette};
use crate::views::{self, ViewScratch, at, titlebar, window_origin, window_rect};
use egui::text::{LayoutJob, TextWrapping};
use egui::{CornerRadius, CursorIcon, Id, Pos2, Rect, Sense, Ui, pos2, vec2};
use fxsound_core::ViewMode;

/// The notice strip's JUCE font height: the readout strip's size, a point up.
pub const NOTICE_FONT_PX: f32 = 12.0;

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

    views::preset_combo(
        ui,
        state,
        palette,
        assets,
        at(origin, layout::lite::preset_combo()),
        &mut response,
    );
    views::device_combo(
        ui,
        state,
        palette,
        assets,
        at(origin, layout::lite::output_combo()),
        &mut response,
    );

    if let Some(text) = &state.notification {
        notice_strip(
            ui,
            text,
            palette,
            at(origin, layout::lite::notification()),
            &mut response,
        );
    }

    response
}

/// The notice as one line: centred under the combos, elided when it is longer than they are wide.
/// Returns where the text went.
fn notice_strip(
    ui: &Ui,
    text: &str,
    palette: Palette,
    strip: Rect,
    response: &mut UiResponse,
) -> Rect {
    let colour = palette.color(FxColor::DefaultText);
    // One line only: the first line of a multi-line message, and that elided to the strip.
    let line = text.lines().next().unwrap_or_default();
    let mut job = LayoutJob::single_section(
        line.to_owned(),
        egui::TextFormat::simple(views::pro::caption_font(NOTICE_FONT_PX), colour),
    );
    job.wrap = TextWrapping::truncate_at_width(strip.width());
    let galley = ui.painter().layout_job(job);
    let placed = Rect::from_center_size(strip.center(), galley.size());
    ui.painter().galley(placed.min, galley, colour);

    let hit = ui.interact(strip, Id::new("fx_notice_strip"), Sense::click());
    if hit.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if hit.clicked() {
        response.push(UiAction::DismissNotice);
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PresetEntry;
    use crate::theme;
    use crate::views::testing::{Harness, painted_bounds, text_below, texts};
    use crate::widgets::combo;
    use egui::{Event, RawInput};
    use fxsound_core::{AudioDevice, DeviceDirection, ThemeMode};

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
            selected_output: Some(0),
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
        // that kept a Pro coordinate by accident would hang off it — the notice included, which in
        // the original started 50 points left of the window.
        let window = window_rect(Pos2::ZERO, ViewMode::Lite);
        let long = "A notice far longer than the two combos are wide, which has to be cut short \
                    rather than run past the panel, the window, or anything else in its way.";
        for (label, state) in [
            ("plain", state()),
            (
                "with a notice",
                UiState {
                    notification: Some(long.to_owned()),
                    ..state()
                },
            ),
            (
                "editing the microphone",
                UiState {
                    notification: Some("Preset: Clean Voice".to_owned()),
                    ..lanes()
                },
            ),
        ] {
            for mode in [ThemeMode::Dark, ThemeMode::Light] {
                let mut harness = Harness::new(mode);
                let shapes = harness.settle(&state);
                let painted = painted_bounds(&shapes);
                assert!(painted.is_positive(), "{label}: the window painted nothing");
                assert!(
                    window.expand(1.0).contains_rect(painted),
                    "{label}, {mode:?}: {painted:?} spills out of {window:?}"
                );
                assert!(
                    painted.width() > window.width() - 2.0
                        && painted.height() > window.height() - 2.0,
                    "{label}: only {painted:?} of {window:?} was painted"
                );
            }
        }
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

    // ---- two lanes in one list ---------------------------------------------------------------

    fn device(name: &str, direction: DeviceDirection) -> AudioDevice {
        AudioDevice {
            id: 0,
            name: format!("node.{name}"),
            description: name.to_owned(),
            is_default: false,
            direction,
            form_factor: String::new(),
        }
    }

    /// A speaker and a microphone, both lanes on, the microphone being edited.
    fn lanes() -> UiState {
        UiState {
            devices: vec![
                device("Speakers", DeviceDirection::Output),
                device("Microphone", DeviceDirection::Input),
            ],
            selected_output: Some(0),
            selected_input: Some(1),
            direction: DeviceDirection::Input,
            ..state()
        }
    }

    /// Open the device list and return what its open menu painted.
    ///
    /// On a screen tall enough for the whole menu: in the 189-point window it scrolls, and which
    /// rows are in view is egui's business, not this list's.
    fn open(harness: &mut Harness, state: &UiState) -> Vec<egui::epaint::ClippedShape> {
        harness.screen = Some(egui::vec2(550.0, 700.0));
        harness.click(state, layout::lite::output_combo().center());
        harness.frame(state, Vec::new());
        harness.frame(state, Vec::new()).1
    }

    #[test]
    fn the_list_titles_both_lanes_with_an_off_row_under_each_title() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = lanes();
        let shapes = open(&mut harness, &state);
        let below = layout::lite::output_combo().bottom() - 1.0;
        let output = text_below(&shapes, "Output", below);
        let input = text_below(&shapes, "Input", below);
        let offs = text_below(&shapes, "Off", below);
        let speakers = text_below(&shapes, "Speakers", below);
        let microphone = text_below(&shapes, "Microphone", below);
        assert_eq!((output.len(), input.len()), (1, 1));
        assert_eq!(offs.len(), 2, "one Off per lane");
        // Output, Off, Speakers, Input, Off, Microphone — top to bottom.
        let order = [
            output[0],
            offs[0],
            speakers[0],
            input[0],
            offs[1],
            microphone[0],
        ];
        for pair in order.windows(2) {
            assert!(
                pair[0].top() < pair[1].top(),
                "{:?} is not above {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn picking_off_under_input_detaches_only_the_input() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = lanes();
        let shapes = open(&mut harness, &state);
        let below = layout::lite::output_combo().bottom() - 1.0;
        let input = text_below(&shapes, "Input", below)[0];
        let off = text_below(&shapes, "Off", below)
            .into_iter()
            .find(|rect| rect.top() > input.top())
            .expect("an Off under Input");
        assert_eq!(
            harness.click(&state, off.center()),
            vec![UiAction::DetachInput]
        );
    }

    #[test]
    fn picking_a_speaker_while_editing_the_microphone_edits_the_output() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = lanes();
        let shapes = open(&mut harness, &state);
        let below = layout::lite::output_combo().bottom() - 1.0;
        let speakers = text_below(&shapes, "Speakers", below)[0];
        assert_eq!(
            harness.click(&state, speakers.center()),
            vec![
                UiAction::SetEditDirection(DeviceDirection::Output),
                UiAction::SelectOutput(0)
            ]
        );
    }

    #[test]
    fn opening_the_single_list_does_not_move_the_edit_direction() {
        // It holds both lanes, so a click on it says nothing about which one is wanted.
        let mut harness = Harness::new(ThemeMode::Dark);
        let actions = harness.click(&lanes(), layout::lite::output_combo().center());
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn the_closed_box_shows_the_edit_directions_device_or_off() {
        let combo = layout::lite::output_combo();
        let shown = |state: &UiState| {
            let mut harness = Harness::new(ThemeMode::Dark);
            let shapes = harness.settle(state);
            texts(&shapes)
                .into_iter()
                .filter(|(_, rect, _)| combo.contains(rect.center()))
                .map(|(text, _, _)| text)
                .collect::<Vec<_>>()
        };
        assert_eq!(shown(&lanes()), ["Microphone"]);
        let output = UiState {
            direction: DeviceDirection::Output,
            ..lanes()
        };
        assert_eq!(shown(&output), ["Speakers"]);
        let detached = UiState {
            selected_input: None,
            ..lanes()
        };
        assert_eq!(shown(&detached), ["Off"]);
    }

    // ---- the notice strip --------------------------------------------------------------------

    #[test]
    fn a_notice_is_one_line_under_the_combos_inside_the_panel() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = UiState {
            notification: Some("Preset: Rock".to_owned()),
            ..state()
        };
        let shapes = harness.settle(&state);
        let line = texts(&shapes)
            .into_iter()
            .find(|(text, _, _)| text == "Preset: Rock")
            .expect("the notice is painted");
        let strip = layout::lite::notification();
        assert!(
            strip.contains_rect(line.1),
            "{:?} is outside {strip:?}",
            line.1
        );
        assert!(panel(Pos2::ZERO).contains_rect(line.1));
        assert!(
            (line.1.center().x - strip.center().x).abs() < 1.0,
            "centred"
        );
        assert!(line.1.top() > layout::lite::output_combo().bottom());
    }

    #[test]
    fn no_notice_draws_nothing_under_the_combos() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let shapes = harness.settle(&state());
        let strip = layout::lite::notification();
        assert!(
            texts(&shapes)
                .iter()
                .all(|(_, rect, _)| !strip.contains(rect.center())),
            "text under the combos with no notice"
        );
    }

    #[test]
    fn a_long_or_multi_line_notice_is_cut_to_one_line_that_fits() {
        let mut harness = Harness::new(ThemeMode::Light);
        let text = format!("{}\nsecond line", "word ".repeat(80));
        let state = UiState {
            notification: Some(text),
            ..state()
        };
        let shapes = harness.settle(&state);
        let line = texts(&shapes)
            .into_iter()
            .find(|(text, _, _)| text.starts_with("word"))
            .expect("the notice is painted");
        assert!(!line.0.contains("second line"), "one line only");
        let strip = layout::lite::notification();
        assert!(
            strip.expand(0.5).contains_rect(line.1),
            "{:?} is outside {strip:?}",
            line.1
        );
    }

    #[test]
    fn clicking_the_notice_takes_it_down() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = UiState {
            notification: Some("Preset: Rock".to_owned()),
            ..state()
        };
        harness.settle(&state);
        let actions = harness.click(&state, layout::lite::notification().center());
        assert_eq!(actions, vec![UiAction::DismissNotice]);
    }
}
