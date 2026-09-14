//! The Pro window — 1040 × 588, everything on screen at once.
//!
//! Ports `FxProView` (`fxsound/Source/GUI/FxProView.cpp`) plus the window chrome `FxWindow` paints
//! around it. The content is 1040 × 511 at `(0, 57)` (`FxProView.cpp:69`: `setSize(WIDTH, HEIGHT +
//! 20)`, which is the size once the visualizer is shown — and since v2.0 it always is), the title
//! bar occupies the 56 points above it, and the window is 20 points taller than the content so the
//! rounded bottom corners have somewhere to live.
//!
//! ```text
//! window   (0,   0, 1040, 588)   rounded, radius 21, WindowBackground
//! title    (21,  0,  998,  56)   views::titlebar
//! divider  (0,  56, 1040,   1)   ControlBackground
//! panel    (20, 73, 1000, 487)   rounded, radius 8, PanelBackground α 0.2
//! preset   (40, 89,  470,  40)   output (530, 89, 470, 40)
//! spectrum (40,149,  960, 120)
//! effects  (40,285,  168, 257)   equalizer (224, 285, 776, 257)
//! ```
//!
//! (`docs/spec/01-window-layout.md` §8.1's table, which is also what [`crate::layout::pro`]
//! returns.)
//!
//! ## The effect column
//!
//! `FxAudioControls` is a two-faced panel: face A is the five effect sliders (`FxEffects`), face B
//! the equalizer's own controls — band count, filter width, restore defaults — reached through a
//! flip button (`docs/spec/03-controls.md` §5.1, §5.7). This view draws **face A only**. The
//! equalizer widget can draw face B itself, at the original's offsets, through
//! `EqualizerWidget::with_controls`; wiring the flip that swaps the two is a separate piece of work
//! and is deliberately not started here rather than half-done.
//!
//! ## What `paint()` does that this does not
//!
//! `FxProView::paint` assigns `setEnabled(...)` to its children from the power state
//! (`FxProView.cpp:112-122`) — a mutation inside a paint routine. In immediate mode the same thing
//! falls out for free: [`UiState::controls_enabled`] is read each frame, and the playback-device
//! combo is the one control deliberately left live with the power off.

use crate::assets::AssetCache;
use crate::layout;
use crate::state::{UiAction, UiResponse, UiState};
use crate::theme::{self, FxColor, Palette};
use crate::views::{self, ViewScratch, at, titlebar, window_origin, window_rect};
use fxsound_core::i18n::tr;
use crate::widgets::icon_button;
use crate::widgets::{EqualizerWidget, FxSlider, VisualizerWidget};
use egui::{Align2, CornerRadius, Rect, Ui, Vec2, pos2, vec2};
use fxsound_core::{Effect, ViewMode, scale};

/// The effect captions' JUCE height: `getNormalFont().withHeight(14.0f)`
/// (`FxAudioControls.cpp:104`).
///
/// `juce::Font::withHeight` sets **ascent + descent** to this many pixels, which is exactly why the
/// caption's label box is also 14 points tall — the glyphs fill it. `egui::FontId::size` is the em
/// size instead, about 1.2 times smaller for Gilroy, so the number has to be converted rather than
/// copied (`docs/spec/01-window-layout.md` §2.3, [`icon_button::JUCE_HEIGHT_PER_EM`]). Passing 14
/// straight to `FontId::new` would lay out a ~17 point line inside a 14 point box and push the
/// descenders into the slider below it; the test at the bottom of this file is what keeps that
/// from creeping back.
pub const CAPTION_FONT_PX: f32 = 14.0;

/// The floating value readout's JUCE height: `getNormalFont().withHeight(12.0f)`
/// (`FxAudioControls.cpp:188`), converted the same way.
pub const VALUE_FONT_PX: f32 = 12.0;

/// A [`FontId`](egui::FontId) for a JUCE font height, in Gilroy Semibold — `getNormalFont()`.
#[must_use]
pub fn caption_font(juce_height_px: f32) -> egui::FontId {
    theme::semibold(juce_height_px / icon_button::JUCE_HEIGHT_PER_EM)
}

/// Geometry inside the 168 × 257 effect column.
///
/// `FxAudioControls.h:64-68` gives the constants and `FxAudioControls.cpp:141-152` the loop;
/// `docs/spec/03-controls.md` §4.6 resolves both into the five rows this module paints.
pub mod effects {
    use super::{Rect, Vec2, pos2, vec2};
    use crate::layout::audio_controls;
    use crate::widgets::slider;

    /// `FxEffects::X_MARGIN` — where a slider starts inside the column.
    pub const X_MARGIN: f32 = audio_controls::X_MARGIN;
    /// `FxEffects::Y_MARGIN` — where the first caption starts.
    pub const Y_MARGIN: f32 = audio_controls::Y_MARGIN;
    /// `FxEffects::LABEL_HEIGHT`.
    pub const CAPTION_HEIGHT: f32 = audio_controls::LABEL_HEIGHT;
    /// `FxEffects::SLIDER_WIDTH` × `FxEffects::SLIDER_HEIGHT`.
    pub const SLIDER_SIZE: Vec2 = vec2(audio_controls::SLIDER_WIDTH, audio_controls::SLIDER_HEIGHT);
    /// `slider.bounds = (X_MARGIN, label.bottom + 1, …)`.
    pub const CAPTION_GAP: f32 = 1.0;
    /// `y = slider.bottom + 10` before the next row.
    pub const ROW_GAP: f32 = 10.0;
    /// One row's full height: caption, gap, slider, gap.
    pub const ROW_PITCH: f32 = CAPTION_HEIGHT + CAPTION_GAP + SLIDER_SIZE.y + ROW_GAP;

    /// The value readout: `SLIDER_THUMB_RADIUS * 3` wide by `LABEL_HEIGHT` high
    /// (`FxAudioControls.cpp:229`).
    pub const VALUE_LABEL_SIZE: Vec2 = vec2(slider::THUMB_RADIUS * 3.0, 12.0);
    /// `pos(value) + SLIDER_THUMB_RADIUS + 1` (`FxAudioControls.cpp:204`, `:244`).
    pub const VALUE_LABEL_GAP: f32 = 1.0;
    /// `juce::Label`'s default `BorderSize<int>(1, 5, 1, 5)`: the glyphs start five points inside
    /// the label's own rectangle. The captions zero this inset explicitly
    /// (`FxAudioControls.cpp:104-108`); the value readouts do not.
    pub const LABEL_BORDER_LEFT: f32 = 5.0;

    /// Top of row `index` inside the column.
    #[must_use]
    pub fn row_top(column: Rect, index: usize) -> f32 {
        column.top() + Y_MARGIN + ROW_PITCH * index as f32
    }

    /// Row `index`'s caption.
    ///
    /// Its x is `X_MARGIN + SLIDER_THUMB_RADIUS`, which lines the text up with the centre of the
    /// thumb at value 0 rather than with the slider's left edge — and, at 160 points wide starting
    /// eight further in than the slider, overhangs the 168 point column by eight. Harmless: the
    /// captions are short and left-justified.
    #[must_use]
    pub fn caption_rect(column: Rect, index: usize) -> Rect {
        Rect::from_min_size(
            pos2(
                column.left() + X_MARGIN + slider::THUMB_RADIUS,
                row_top(column, index),
            ),
            vec2(SLIDER_SIZE.x, CAPTION_HEIGHT),
        )
    }

    /// Row `index`'s slider.
    #[must_use]
    pub fn slider_rect(column: Rect, index: usize) -> Rect {
        Rect::from_min_size(
            pos2(
                column.left() + X_MARGIN,
                caption_rect(column, index).bottom() + CAPTION_GAP,
            ),
            SLIDER_SIZE,
        )
    }

    /// The readout that floats to the right of the thumb, for a value at proportion `t` of the
    /// slider's range.
    #[must_use]
    pub fn value_label_rect(slider_rect: Rect, t: f32) -> Rect {
        let track = slider::track_rect(slider_rect);
        let thumb_x = track.left() + track.width() * t.clamp(0.0, 1.0);
        Rect::from_min_size(
            pos2(
                thumb_x + slider::THUMB_RADIUS + VALUE_LABEL_GAP,
                slider_rect.top() + ((slider_rect.height() - VALUE_LABEL_SIZE.y) / 2.0).floor(),
            ),
            VALUE_LABEL_SIZE,
        )
    }
}

/// Paint the Pro window and report what the user did.
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    scratch: &mut ViewScratch,
    palette: Palette,
    assets: &mut AssetCache,
) -> UiResponse {
    let origin = window_origin(ui);

    // `setOpaque(false)` plus a rounded fill is what makes the corners round instead of black
    // (`FxWindow.cpp:131-139`); the backend clear colour must be transparent for it to show.
    ui.painter().rect_filled(
        window_rect(origin, ViewMode::Pro),
        CornerRadius::same(layout::WINDOW_CORNER_RADIUS as u8),
        palette.window_background(),
    );
    // `fillRoundedRectangle(20, 16, 1000, 347 + 140, 8)` (`FxProView.cpp:110-114`).
    ui.painter().rect_filled(
        at(origin, layout::pro::panel()),
        CornerRadius::same(layout::PANEL_CORNER_RADIUS as u8),
        palette.panel_background(),
    );

    let mut response = titlebar::show(ui, state, scratch, palette, assets);

    views::combos(
        ui,
        state,
        palette,
        assets,
        at(origin, layout::pro::preset_combo()),
        at(origin, layout::pro::output_combo()),
        &mut response,
    );

    VisualizerWidget::new(state, &mut scratch.visualizer).show(
        ui,
        at(origin, layout::pro::visualizer()),
        palette,
    );

    effect_column(
        ui,
        state,
        palette,
        assets,
        at(origin, layout::pro::audio_controls()),
        &mut response,
    );

    EqualizerWidget::new(state, &mut scratch.eq).show(
        ui,
        at(origin, layout::pro::equalizer()),
        palette,
        assets,
        &mut response,
    );

    response
}

/// The five effect sliders, their captions and their value readouts (`FxEffects`,
/// `FxAudioControls.cpp:88-247`).
fn effect_column(
    ui: &mut Ui,
    state: &UiState,
    palette: Palette,
    assets: &mut AssetCache,
    column: Rect,
    response: &mut UiResponse,
) {
    let enabled = state.controls_enabled();
    let caption_colour = palette.color(FxColor::DefaultText);
    let value_colour = palette.color(FxColor::HighlightedText);

    for (index, effect) in Effect::ALL.into_iter().enumerate() {
        let caption = effects::caption_rect(column, index);
        // `FxEffects::paint` re-applies the translated caption every repaint so a language switch
        // propagates (`FxAudioControls.cpp:154-178`); reading the string each frame is the same
        // thing, for free.
        ui.painter().text(
            caption.left_top(),
            Align2::LEFT_TOP,
            tr(effect.label()),
            caption_font(CAPTION_FONT_PX),
            caption_colour,
        );

        let rect = effects::slider_rect(column, index);
        let mut value = state.effect(effect);
        // 0…10 in whole steps (`FxAudioControls.cpp:113`). The five effect sliders are the ones
        // *without* right-click-to-reset — that belongs to `FxAudioSlider` and `FxBalanceSlider`
        // (`docs/spec/03-controls.md` §3.5).
        let slider = FxSlider::new(&mut value, 0.0, scale::SLIDER_MAX, 1.0)
            .enabled(enabled)
            .show(ui, rect, palette, assets, effect.key());
        let changed = slider.changed();
        // The five help tips (`FxAudioControls.cpp:157-161`), cleared while the user has ticked
        // "Hide help tips for audio controls" (`:169-176`).
        if !state.hide_tooltips {
            let _ = slider.on_hover_text(tr(effect.tooltip()));
        }
        if changed {
            response.push(UiAction::SetEffect(effect, value));
        }

        // `showValue(show)` is `show && isEnabled()` (`FxAudioControls.cpp:208-211`), and since
        // v2.0 `show` is unconditionally true so touch users can read the value
        // (`FxProView.cpp:70`).
        if enabled {
            let t = value / scale::SLIDER_MAX;
            let label = effects::value_label_rect(rect, t);
            ui.painter().text(
                pos2(label.left() + effects::LABEL_BORDER_LEFT, label.center().y),
                Align2::LEFT_CENTER,
                format!("{value:.0}"),
                caption_font(VALUE_FONT_PX),
                value_colour,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PresetEntry;
    use crate::widgets::slider;
    use egui::{Event, PointerButton, Pos2, RawInput};
    use fxsound_core::{AudioDevice, ThemeMode};

    fn column() -> Rect {
        layout::pro::audio_controls()
    }

    fn state() -> UiState {
        UiState {
            view: ViewMode::Pro,
            presets: vec![
                PresetEntry {
                    name: "Flat".to_owned(),
                    factory: true,
                    modified: false,
                },
                PresetEntry {
                    name: "My Mix".to_owned(),
                    factory: false,
                    modified: true,
                },
            ],
            selected_preset: Some(0),
            devices: vec![AudioDevice {
                id: 42,
                name: "alsa_output.pci-0000_00_1f.3.analog-stereo".to_owned(),
                description: "Built-in Audio Analogue Stereo".to_owned(),
                is_default: true,
                direction: fxsound_core::DeviceDirection::Output,
            }],
            selected_device: Some(0),
            effects: [3.0, 5.0, 7.0, 4.0, 8.0],
            ..UiState::default()
        }
    }

    fn test_context() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions());
        ctx
    }

    fn raw_input(events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                views::window_size(ViewMode::Pro),
            )),
            events,
            ..Default::default()
        }
    }

    fn frame(
        ctx: &egui::Context,
        state: &UiState,
        scratch: &mut ViewScratch,
        assets: &mut AssetCache,
        events: Vec<Event>,
    ) -> Vec<UiAction> {
        let mut actions = Vec::new();
        ctx.run_ui(raw_input(events), |ui| {
            actions = show(ui, state, scratch, Palette::new(ThemeMode::Dark), assets).actions;
        })
        .drop_without_applying_deltas();
        actions
    }

    #[test]
    fn the_five_rows_land_on_the_specs_resolved_grid() {
        // docs/spec/03-controls.md §4.6, column-local: captions at x = 16 and y = 21, 64, 107,
        // 150, 193; sliders at x = 8, fifteen points under each caption.
        let column = column();
        for (index, caption_y) in [21.0_f32, 64.0, 107.0, 150.0, 193.0]
            .into_iter()
            .enumerate()
        {
            let caption = effects::caption_rect(column, index);
            assert!(
                (caption.left() - (column.left() + 16.0)).abs() < 1e-4,
                "{caption:?}"
            );
            assert!(
                (caption.top() - (column.top() + caption_y)).abs() < 1e-4,
                "{caption:?}"
            );
            assert!((caption.width() - 160.0).abs() < 1e-4);
            assert!((caption.height() - 14.0).abs() < 1e-4);

            let slider = effects::slider_rect(column, index);
            assert!(
                (slider.left() - (column.left() + 8.0)).abs() < 1e-4,
                "{slider:?}"
            );
            assert!(
                (slider.top() - (column.top() + caption_y + 15.0)).abs() < 1e-4,
                "{slider:?}"
            );
            assert!((slider.width() - 160.0).abs() < 1e-4);
            assert!((slider.height() - 18.0).abs() < 1e-4);
        }
    }

    #[test]
    fn the_last_slider_leaves_thirty_one_points_at_the_bottom_of_the_column() {
        // §4.6: last slider bottom = 226 of the 257 point panel.
        let column = column();
        let last = effects::slider_rect(column, Effect::COUNT - 1);
        assert!(
            (last.bottom() - (column.top() + 226.0)).abs() < 1e-4,
            "{last:?}"
        );
        assert!((column.bottom() - last.bottom() - 31.0).abs() < 1e-4);
    }

    #[test]
    fn the_slider_runs_flush_to_the_columns_right_edge_and_the_caption_overhangs_it() {
        let column = column();
        let slider = effects::slider_rect(column, 0);
        assert!((slider.right() - column.right()).abs() < 1e-4, "{slider:?}");
        // The caption starts eight points further in and is the same width, so it hangs over.
        let caption = effects::caption_rect(column, 0);
        assert!(
            (caption.right() - column.right() - 8.0).abs() < 1e-4,
            "{caption:?}"
        );
    }

    #[test]
    fn the_value_readout_tracks_the_thumb_across_the_track() {
        // §4.5: x = pos(value) + 9, i.e. 17 at the minimum and 129 at the maximum, measured from
        // the slider's own left edge.
        let rect = effects::slider_rect(column(), 0);
        for (t, expected) in [(0.0_f32, 17.0_f32), (0.5, 73.0), (1.0, 129.0)] {
            let label = effects::value_label_rect(rect, t);
            assert!(
                (label.left() - rect.left() - expected).abs() < 1e-4,
                "t = {t} gave {}",
                label.left() - rect.left()
            );
        }
        // 24 x 12, vertically centred in the 18 point slider.
        let label = effects::value_label_rect(rect, 0.0);
        assert!((label.width() - 24.0).abs() < 1e-4);
        assert!((label.height() - 12.0).abs() < 1e-4);
        assert!((label.top() - rect.top() - 3.0).abs() < 1e-4);
    }

    #[test]
    fn the_readout_never_escapes_its_slider_even_at_full_scale() {
        let rect = effects::slider_rect(column(), 0);
        let label = effects::value_label_rect(rect, 1.0);
        assert!(
            label.right() <= rect.right() + 1e-4,
            "{label:?} vs {rect:?}"
        );
    }

    #[test]
    fn the_row_pitch_is_the_sum_of_the_parts_the_original_adds_up() {
        // 14 caption + 1 gap + 18 slider + 10 gap.
        assert!((effects::ROW_PITCH - 43.0).abs() < 1e-6);
        let column = column();
        assert!((effects::row_top(column, 1) - effects::row_top(column, 0) - 43.0).abs() < 1e-4);
    }

    #[test]
    fn every_effect_row_stays_inside_the_panel_behind_it() {
        let panel = layout::pro::panel();
        let column = column();
        for index in 0..Effect::COUNT {
            assert!(
                panel.contains_rect(effects::slider_rect(column, index)),
                "row {index} escaped the panel"
            );
        }
    }

    #[test]
    fn the_captions_and_readouts_fit_the_boxes_juce_measured_them_into() {
        // `withHeight(h)` makes a JUCE line exactly `h` pixels tall, and both label boxes are sized
        // from the same number — 14 for the caption, 12 for the readout. Copying those into
        // `FontId::new` instead of converting them would lay out a line half again too tall and
        // spill the captions into the sliders.
        let ctx = test_context();
        ctx.run_ui(raw_input(Vec::new()), |ui| {
            for (juce_px, box_height, sample) in [
                (CAPTION_FONT_PX, effects::CAPTION_HEIGHT, "Dynamic Boost"),
                (VALUE_FONT_PX, effects::VALUE_LABEL_SIZE.y, "10"),
            ] {
                let galley = ui.painter().layout_no_wrap(
                    sample.to_owned(),
                    caption_font(juce_px),
                    egui::Color32::PLACEHOLDER,
                );
                assert!(
                    galley.size().y <= box_height + 0.5,
                    "{sample:?} lays out {} points tall in a {box_height} point box",
                    galley.size().y
                );
                // …and not so small that the box is mostly empty either.
                assert!(
                    galley.size().y >= box_height - 3.0,
                    "{sample:?} is {}",
                    galley.size().y
                );
            }
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn nothing_the_window_paints_escapes_the_window() {
        // The window is not resizable and has no scroll area anywhere in it, so anything painted
        // outside its 1040 x 588 is simply lost — and on a transparent, undecorated surface it is
        // lost silently. Tessellating a real frame is the cheapest way to keep that honest.
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = state();
        let window = Rect::from_min_size(Pos2::ZERO, views::window_size(ViewMode::Pro));

        // One warm-up frame so the SVG textures exist, then the frame that is measured.
        ctx.run_ui(raw_input(Vec::new()), |ui| {
            show(
                ui,
                &state,
                &mut scratch,
                Palette::new(ThemeMode::Dark),
                &mut assets,
            );
        })
        .drop_without_applying_deltas();

        let output = ctx.run_ui(raw_input(Vec::new()), |ui| {
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
        // A point of slack for the feathering epaint puts around every antialiased edge.
        assert!(
            window.expand(1.0).contains_rect(painted),
            "{painted:?} spills out of {window:?}"
        );
        // …and it really did cover the window, rather than passing by painting almost nothing.
        assert!(
            painted.width() > window.width() - 2.0 && painted.height() > window.height() - 2.0,
            "only {painted:?} of {window:?} was painted"
        );
    }

    #[test]
    fn a_quiet_pro_frame_reports_nothing() {
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
    fn clicking_the_middle_of_an_effect_track_sets_that_effect_to_five() {
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = state();

        // Clarity starts at 3; the centre of its track is value 5.
        let rect = effects::slider_rect(column(), 0);
        let target = slider::track_rect(rect).center();

        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(target)],
            vec![
                Event::PointerMoved(target),
                Event::PointerButton {
                    pos: target,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        ] {
            actions.extend(frame(&ctx, &state, &mut scratch, &mut assets, events));
        }

        assert_eq!(actions, vec![UiAction::SetEffect(Effect::Fidelity, 5.0)]);
    }

    #[test]
    fn the_power_state_gates_the_effect_column_but_not_the_output_list() {
        // `FxProView::paint` disables the preset list, the audio controls, the EQ and the
        // visualizer, and deliberately leaves `endpoint_list_` alone (`FxProView.cpp:117-123`).
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = UiState {
            power: false,
            ..state()
        };
        assert!(!state.controls_enabled());

        // Pressing on a dead slider must not move it.
        let rect = effects::slider_rect(column(), 0);
        let target = slider::track_rect(rect).center();
        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(target)],
            vec![
                Event::PointerMoved(target),
                Event::PointerButton {
                    pos: target,
                    button: PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        ] {
            actions.extend(frame(&ctx, &state, &mut scratch, &mut assets, events));
        }
        assert!(
            actions.is_empty(),
            "a powered-down slider moved: {actions:?}"
        );
    }

    #[test]
    fn the_visualizer_keeps_animating_across_frames() {
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = UiState {
            audio_active: true,
            spectrum: [0.75; fxsound_core::NUM_SPECTRUM_BARS],
            ..state()
        };
        assert!(scratch.visualizer.is_settled());
        for _ in 0..8 {
            frame(&ctx, &state, &mut scratch, &mut assets, Vec::new());
        }
        assert!(
            !scratch.visualizer.is_settled(),
            "the spectrum strip never took the live frame"
        );
    }
}
