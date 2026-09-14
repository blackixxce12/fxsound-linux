//! The 56 point title bar shared by both windows.
//!
//! Ports `FxWindow::TitleBar` (`fxsound/Source/GUI/FxWindow.cpp:261-395`) together with the button
//! set `FxMainWindow` pushes into it (`FxMainWindow.cpp:230-234`). The bar is a child component
//! inset 21 points on each side — the width of the window's rounded corners — and 56 points tall,
//! with a one point rule under it at y = 56 (`FxWindow.cpp:141`, `:145-152`).
//!
//! ## Where the numbers come from
//!
//! Nowhere in this file. `FxWindow::TitleBar::resized` walks its buttons in insertion order,
//! stepping a running `x_right` / `x_left` by `button width + 20` each time
//! (`FxWindow.cpp:261-310`); that walk is already resolved into [`crate::layout::Chrome`], whose
//! two constants are the spec's §4.2 tables verbatim. This module only chooses which `Chrome` the
//! current view uses and paints into its rectangles.
//!
//! ## The drag region
//!
//! `FxWindow::TitleBar::mouseDown` starts a `ComponentDragger` on the whole bar, and JUCE's child
//! components — the buttons — swallow the press before it gets there (`FxWindow.cpp:339-358`).
//! egui has no children, so the same effect is produced by hit-test order: the bar is registered
//! **first**, every button after it, and egui's hit test breaks a tie in favour of the widget
//! registered last (`egui-0.36.0/src/hit_test.rs`, `find_closest_within`: *"In case of a tie, take
//! the last one = the one on top"*). The bar senses `click_and_drag` rather than `drag` alone
//! precisely so that a pure-click button on top of it still wins the click — with a drag-only
//! background egui refuses to report a click on a widget that is not fully contained by it, and
//! the close button's enlarged hit area hangs past the bar's right edge.
//!
//! Two deliberate differences from the original:
//!
//! * **The wordmark is draggable.** In JUCE the logo is a `Drawable` *component* and therefore
//!   intercepts the press, leaving a 106 point dead strip at the left of the bar
//!   (`docs/spec/01-window-layout.md` §3.8). Here it is painted artwork, not a widget, so the
//!   whole bar drags.
//! * **The window move is not performed here.** [`UiAction::DragWindow`] leaves for the
//!   application layer, which turns it into `egui::ViewportCommand::StartDrag` — the only thing
//!   that moves a window on Wayland (`docs/api/egui-0.36-viewport.md` §13). It is emitted on
//!   `drag_started` and not once per frame, because `StartDrag` re-arms the compositor's grab
//!   every time it is sent.

use crate::assets::{AssetCache, FxImage};
use crate::layout::{self, Chrome};
use crate::state::{UiAction, UiResponse, UiState};
use crate::theme::{FxColor, Palette};
use crate::views::{ViewScratch, at, window_origin, window_size};
use crate::widgets::PowerButton;
use crate::widgets::icon_button::{self, IconButton};
use egui::{CornerRadius, CursorIcon, Id, Painter, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};
use fxsound_core::ViewMode;

/// `FxWindow::CLOSE_BUTTON_WIDTH` (`FxWindow.h:45`) — the ✕ is a 15 point square.
pub const CLOSE_SIZE: f32 = 15.0;

/// Thickness of each ✕ diagonal as a fraction of the glyph's side (`FxWindow.cpp:164-165`).
///
/// The original builds the path in a unit square and scales it to a square of side = the
/// component's height, so at the shipping 15 points the strokes are 1.2 points thick.
pub const CLOSE_STROKE_RATIO: f32 = 0.08;

/// How long the wordmark takes to cross-fade when DSP processing starts or stops
/// (`FxWindow.cpp:193-208`: `fadeOut`/`fadeIn` at 600 ms).
pub const LOGO_FADE_SECS: f32 = 0.6;

/// The `Chrome` a view lays its title bar out with.
#[must_use]
pub const fn chrome(view: ViewMode) -> Chrome {
    match view {
        ViewMode::Pro => Chrome::PRO,
        ViewMode::Lite => Chrome::LITE,
    }
}

/// The flip button's two glyphs: a "shrink" arrow while in Pro, an "expand" one while in Lite
/// (`FxMainWindow::setResizeImage`, `FxMainWindow.cpp:349-365`).
///
/// Note that the *Pro* button uses `minimize.svg` and the *Lite* one `maximize.svg`, which reads
/// backwards until you remember the glyph describes what the click will do, not where you are.
#[must_use]
pub const fn flip_images(view: ViewMode) -> (FxImage, FxImage) {
    match view {
        ViewMode::Pro => (FxImage::MinimizeButton, FxImage::MinimizeButtonHover),
        ViewMode::Lite => (FxImage::MaximizeButton, FxImage::MaximizeButtonHover),
    }
}

/// The title bar's own rectangle: inset by the corner radius, 56 points tall
/// (`FxWindow.cpp:145-152`).
#[must_use]
pub fn bar_rect(origin: Pos2, window_width: f32) -> Rect {
    Rect::from_min_size(
        origin + Vec2::new(layout::TITLE_BAR_INSET, 0.0),
        Vec2::new(
            (window_width - layout::TITLE_BAR_INSET * 2.0).max(0.0),
            layout::TITLE_BAR_HEIGHT,
        ),
    )
}

/// The one point rule under the bar (`FxWindow.cpp:141`).
///
/// The original draws it from `shadow_width` to `getWidth() - shadow_width * 2`, which for a
/// dialog overshoots by the shadow width; `docs/spec/01-window-layout.md` §3.3 asks the port to
/// draw the corrected version, and the main window's shadow width is zero anyway.
#[must_use]
pub fn divider_rect(origin: Pos2, window_width: f32) -> Rect {
    Rect::from_min_size(
        origin + Vec2::new(0.0, layout::TITLE_BAR_HEIGHT),
        Vec2::new(window_width, layout::TITLE_BAR_DIVIDER_HEIGHT),
    )
}

/// How thick the ✕'s diagonals are for a glyph of the given side.
#[must_use]
pub fn close_stroke_width(size: f32) -> f32 {
    size * CLOSE_STROKE_RATIO
}

/// Whether a press at `point` starts a window move.
///
/// The draggable strip is the bar minus the buttons' hit areas, which is what JUCE gets for free
/// by making the buttons child components (`FxWindow.cpp:339-358`). egui needs it spelled out,
/// because its hit test hands the press to the button for clicking *and* to the bar for dragging
/// at the same time: with the click already claimed by something else the bar's drag is no longer
/// postponed and starts on the very first pressed frame
/// (`egui-0.36.0/src/interaction.rs:195-211` — `could_still_be_clicked`). Without this test,
/// press-and-hold on the power button would move the window.
#[must_use]
pub fn is_draggable(chrome: Chrome, origin: Pos2, point: Pos2) -> bool {
    chrome.buttons().iter().all(|button| {
        !icon_button::hit_rect(at(origin, button.rect()), icon_button::MIN_HIT_SIZE).contains(point)
    })
}

/// One frame of the wordmark cross-fade.
///
/// `processing` is the flag `FxController` drives the animation from — audio is actually being
/// enhanced — and the fade is linear over [`LOGO_FADE_SECS`] in both directions, so reversing it
/// mid-way costs the distance already travelled rather than restarting.
#[must_use]
pub fn logo_fade(previous: f32, processing: bool, dt: f32) -> f32 {
    let target = if processing { 1.0 } else { 0.0 };
    let previous = if previous.is_finite() {
        previous.clamp(0.0, 1.0)
    } else {
        target
    };
    let step = if dt.is_finite() && dt > 0.0 {
        dt / LOGO_FADE_SECS
    } else {
        0.0
    };
    if previous < target {
        (previous + step).min(target)
    } else {
        (previous - step).max(target)
    }
}

/// Draw the ✕ the way `FxWindow::CloseButton::paintButton` does (`FxWindow.cpp:154-169`).
///
/// It is not artwork: the original fills the component with the window background, builds a path of
/// two line segments across a unit square, scales that path to a square of side = the component's
/// height and fills it with `ImageButton` — dark `#e63462`, light `#23b6eb`. There is no hover and
/// no pressed state; `paintButton` ignores both flags it is handed.
pub fn paint_close_glyph(painter: &Painter, rect: Rect, palette: Palette) {
    painter.rect_filled(
        rect,
        CornerRadius::ZERO,
        palette.color(FxColor::WindowBackground),
    );
    let side = rect.height();
    let glyph = Rect::from_center_size(rect.center(), Vec2::splat(side));
    let stroke = Stroke::new(
        close_stroke_width(side),
        palette.color(FxColor::ImageButton),
    );
    painter.line_segment([glyph.left_top(), glyph.right_bottom()], stroke);
    painter.line_segment([glyph.right_top(), glyph.left_bottom()], stroke);
}

/// Paint the bar and report what the user did to it.
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    scratch: &mut ViewScratch,
    palette: Palette,
    assets: &mut AssetCache,
) -> UiResponse {
    let mut response = UiResponse::default();
    let origin = window_origin(ui);
    let width = window_size(state.view).x;
    let chrome = chrome(state.view);

    ui.painter().rect_filled(
        divider_rect(origin, width),
        CornerRadius::ZERO,
        palette.color(FxColor::ControlBackground),
    );

    drag_region(
        ui,
        scratch,
        chrome,
        origin,
        bar_rect(origin, width),
        &mut response,
    );
    paint_logo(
        ui,
        state,
        scratch,
        palette,
        assets,
        at(origin, chrome.logo.rect()),
    );

    if icon(
        ui,
        state,
        palette,
        assets,
        at(origin, chrome.menu.rect()),
        FxImage::MenuButton,
        FxImage::MenuButtonHover,
        "menu",
    )
    .clicked()
    {
        response.push(UiAction::OpenMenu);
    }

    if PowerButton::new(state.power)
        .hide_tooltips(state.hide_tooltips)
        .show(
            ui,
            at(origin, chrome.power.rect()),
            palette,
            assets,
            "power",
        )
        .clicked()
    {
        response.push(UiAction::TogglePower);
    }

    let (flip_normal, flip_hover) = flip_images(state.view);
    if icon(
        ui,
        state,
        palette,
        assets,
        at(origin, chrome.flip.rect()),
        flip_normal,
        flip_hover,
        "flip",
    )
    .clicked()
    {
        response.push(UiAction::ToggleView);
    }

    if icon(
        ui,
        state,
        palette,
        assets,
        at(origin, chrome.minimize.rect()),
        FxImage::MinimizeWindowButton,
        FxImage::MinimizeWindowButtonHover,
        "minimize",
    )
    .clicked()
    {
        response.push(UiAction::Minimise);
    }

    if close(ui, palette, at(origin, chrome.close.rect())).clicked() {
        response.push(UiAction::Close);
    }

    response
}

/// Everything in the bar that is not a button moves the window.
fn drag_region(
    ui: &mut Ui,
    scratch: &mut ViewScratch,
    chrome: Chrome,
    origin: Pos2,
    bar: Rect,
    response: &mut UiResponse,
) {
    let drag = ui.interact(bar, Id::new("fx_title_bar_drag"), Sense::click_and_drag());
    if drag.drag_started()
        && drag
            .interact_pointer_pos()
            .is_some_and(|point| is_draggable(chrome, origin, point))
    {
        scratch.window_drag = true;
        response.push(UiAction::DragWindow);
    }
    if drag.drag_stopped() {
        scratch.window_drag = false;
    }
}

/// The wordmark, cross-faded between its plain and highlighted variants.
///
/// `FxWindow` keeps two `Drawable`s stacked at the same 106 × 15 rectangle and animates their
/// alphas in opposite directions (`FxWindow.cpp:181`, `:193-208`); painting both at complementary
/// opacities is the same picture. While the fade is running the context is asked to repaint, so a
/// window with no other animation still finishes the transition.
fn paint_logo(
    ui: &Ui,
    state: &UiState,
    scratch: &mut ViewScratch,
    palette: Palette,
    assets: &mut AssetCache,
    rect: Rect,
) {
    let dt = ui.input(|input| input.stable_dt);
    // The original starts the animation from `FxController` when the DSP begins processing, which
    // is audio flowing *and* the power on (`docs/spec/01-window-layout.md` §3.7).
    scratch.logo_fade = logo_fade(scratch.logo_fade, state.audio_active && state.power, dt);
    let fade = scratch.logo_fade;

    if fade < 1.0 {
        icon_button::paint_image(
            ui,
            rect,
            FxImage::DefaultLogo,
            palette.mode(),
            assets,
            1.0 - fade,
        );
    }
    if fade > 0.0 {
        icon_button::paint_image(
            ui,
            rect,
            FxImage::HighlightedLogo,
            palette.mode(),
            assets,
            fade,
        );
    }
    if fade > 0.0 && fade < 1.0 {
        // Paced, not immediate: the window runs without vsync (the compositor withholds frame
        // callbacks from a hidden surface, and a blocking swap would hang the event loop), so an
        // unconditional repaint would spin at whatever rate the GPU allows for the whole fade.
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(16));
    }
}

/// One `DrawableButton` from the bar.
///
/// No button in this bar carries a tooltip: the original gives each of them `setHelpText`, which
/// is accessibility text rather than a hover bubble (`FxMainWindow.cpp:194`, `:200`, `:205`,
/// `:221`). `hide_tooltips` is still threaded through so the setting reaches the widget the same
/// way it does everywhere else.
#[allow(clippy::too_many_arguments)]
fn icon(
    ui: &mut Ui,
    state: &UiState,
    palette: Palette,
    assets: &mut AssetCache,
    rect: Rect,
    normal: FxImage,
    hover: FxImage,
    id_salt: &'static str,
) -> Response {
    IconButton::new(normal)
        .hover(hover)
        .hide_tooltips(state.hide_tooltips)
        .show(ui, rect, palette, assets, id_salt)
}

/// The hand-drawn ✕, with the enlarged hit area `docs/spec/01-window-layout.md` §4.2 invites.
fn close(ui: &mut Ui, palette: Palette, rect: Rect) -> Response {
    let response = ui
        .interact(
            icon_button::hit_rect(rect, icon_button::MIN_HIT_SIZE),
            Id::new("fx_close_button"),
            Sense::click(),
        )
        .on_hover_cursor(CursorIcon::PointingHand);
    paint_close_glyph(ui.painter(), rect, palette);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PresetEntry;
    use crate::theme;
    use egui::{Event, PointerButton, Pos2, RawInput, pos2};
    use fxsound_core::ThemeMode;

    fn state(view: ViewMode) -> UiState {
        UiState {
            view,
            presets: vec![PresetEntry {
                name: "Flat".to_owned(),
                factory: true,
                modified: false,
            }],
            selected_preset: Some(0),
            ..UiState::default()
        }
    }

    fn test_context() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions());
        ctx
    }

    fn raw_input(view: ViewMode, events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, window_size(view))),
            events,
            ..Default::default()
        }
    }

    fn press(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// Run one frame of the title bar and return the actions it reported.
    fn frame(
        ctx: &egui::Context,
        state: &UiState,
        scratch: &mut ViewScratch,
        assets: &mut AssetCache,
        events: Vec<Event>,
    ) -> Vec<UiAction> {
        let mut actions = Vec::new();
        ctx.run_ui(raw_input(state.view, events), |ui| {
            actions = show(ui, state, scratch, Palette::new(ThemeMode::Dark), assets).actions;
        })
        .drop_without_applying_deltas();
        actions
    }

    /// Hover, press and release over `pos`, collecting every action across the three frames.
    ///
    /// egui hit-tests against the *previous* pass's widget rectangles, so the first frame only
    /// registers the bar's widgets; the press lands on the second and the click completes on the
    /// third.
    fn click_at(state: &UiState, pos: Pos2) -> Vec<UiAction> {
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(pos)],
            vec![Event::PointerMoved(pos), press(pos, true)],
            vec![press(pos, false)],
        ] {
            actions.extend(frame(&ctx, state, &mut scratch, &mut assets, events));
        }
        actions
    }

    #[test]
    fn the_bar_is_inset_by_the_corner_radius_and_is_fifty_six_points_tall() {
        // docs/spec/01-window-layout.md §8.1: (21, 0, 998, 56) in a 1040 point window.
        let bar = bar_rect(Pos2::ZERO, 1040.0);
        assert!((bar.left() - 21.0).abs() < 1e-4);
        assert!((bar.top()).abs() < 1e-4);
        assert!((bar.width() - 998.0).abs() < 1e-4, "{bar:?}");
        assert!((bar.height() - 56.0).abs() < 1e-4);

        // §8.2: (21, 0, 508, 56) in a 550 point window.
        let bar = bar_rect(Pos2::ZERO, 550.0);
        assert!((bar.width() - 508.0).abs() < 1e-4, "{bar:?}");
    }

    #[test]
    fn the_divider_is_one_point_tall_and_runs_the_whole_window_width() {
        // §8.1: (0, 56, 1040, 1) — the corrected version, not the original's overshoot.
        let divider = divider_rect(Pos2::ZERO, 1040.0);
        assert!((divider.left()).abs() < 1e-4);
        assert!((divider.top() - 56.0).abs() < 1e-4);
        assert!((divider.width() - 1040.0).abs() < 1e-4);
        assert!((divider.height() - 1.0).abs() < 1e-4);
        // It sits immediately under the bar, with no gap and no overlap.
        assert!((divider.top() - bar_rect(Pos2::ZERO, 1040.0).bottom()).abs() < 1e-4);
    }

    #[test]
    fn the_close_glyph_is_two_diagonals_one_point_two_points_thick() {
        // FxWindow.cpp:164-165 — thickness 0.08 of a unit square scaled to the 15 point component.
        assert!((close_stroke_width(CLOSE_SIZE) - 1.2).abs() < 1e-4);
        assert!((CLOSE_SIZE - Chrome::PRO.close.size.x).abs() < 1e-6);
        assert!((CLOSE_SIZE - Chrome::PRO.close.size.y).abs() < 1e-6);
    }

    #[test]
    fn the_flip_glyph_says_shrink_in_pro_and_expand_in_lite() {
        // FxMainWindow.cpp:349-365 — the glyph describes the click, not the current view.
        assert_eq!(
            flip_images(ViewMode::Pro),
            (FxImage::MinimizeButton, FxImage::MinimizeButtonHover)
        );
        assert_eq!(
            flip_images(ViewMode::Lite),
            (FxImage::MaximizeButton, FxImage::MaximizeButtonHover)
        );
    }

    #[test]
    fn each_view_takes_its_own_chrome_and_they_differ_on_the_right_hand_buttons() {
        assert_eq!(chrome(ViewMode::Pro), Chrome::PRO);
        assert_eq!(chrome(ViewMode::Lite), Chrome::LITE);
        // The wordmark is at the same place in both; the flip button is not the same size.
        assert_eq!(chrome(ViewMode::Pro).logo, chrome(ViewMode::Lite).logo);
        assert!((chrome(ViewMode::Pro).flip.size.x - 26.0).abs() < 1e-6);
        assert!((chrome(ViewMode::Lite).flip.size.x - 24.0).abs() < 1e-6);
    }

    #[test]
    fn the_drag_region_is_the_bar_with_the_buttons_punched_out() {
        let chrome = Chrome::PRO;
        // Empty bar, in the long gap between the hamburger and the power button.
        assert!(is_draggable(chrome, Pos2::ZERO, pos2(500.0, 28.0)));
        // …and over the wordmark, which the original's `Drawable` component would have swallowed.
        assert!(is_draggable(
            chrome,
            Pos2::ZERO,
            chrome.logo.rect().center()
        ));
        for button in chrome.buttons() {
            assert!(
                !is_draggable(chrome, Pos2::ZERO, button.rect().center()),
                "{button:?} is inside the drag region"
            );
        }
        // The ✕'s hit area is grown from 15 to 24 points, so the whole of that square is excluded,
        // not just the glyph.
        let close = chrome.close.rect();
        assert!(!is_draggable(
            chrome,
            Pos2::ZERO,
            pos2(close.left() - 3.0, close.center().y)
        ));
    }

    #[test]
    fn the_drag_region_follows_the_window_it_was_measured_against() {
        let origin = pos2(100.0, 50.0);
        // The same point that is a button at the origin is empty bar once the window has moved.
        let power = Chrome::PRO.power.rect().center();
        assert!(!is_draggable(Chrome::PRO, Pos2::ZERO, power));
        assert!(is_draggable(Chrome::PRO, origin, power));
        assert!(!is_draggable(Chrome::PRO, origin, power + origin.to_vec2()));
    }

    #[test]
    fn the_wordmark_crossfades_over_six_hundred_milliseconds() {
        // Eighteen frames of the visualizer's own 30 Hz grid is exactly 0.6 s.
        let dt = 1.0 / 30.0;
        let mut fade = 0.0_f32;
        for _ in 0..17 {
            fade = logo_fade(fade, true, dt);
            assert!(fade < 1.0, "the fade finished early at {fade}");
        }
        fade = logo_fade(fade, true, dt);
        assert!((fade - 1.0).abs() < 1e-4, "ended at {fade}");
        // Half the time is half the distance: the fade is linear.
        let mut half = 0.0_f32;
        for _ in 0..9 {
            half = logo_fade(half, true, dt);
        }
        assert!((half - 0.5).abs() < 1e-3, "halfway was {half}");
    }

    #[test]
    fn the_crossfade_runs_back_from_wherever_it_had_reached() {
        let dt = 0.2;
        let forward = logo_fade(0.0, true, dt);
        assert!((forward - 1.0 / 3.0).abs() < 1e-4, "{forward}");
        // Reversing costs only the distance already travelled, not a fresh 600 ms.
        let back = logo_fade(forward, false, dt);
        assert!(back.abs() < 1e-6, "{back}");
        // It never runs past either end.
        assert!((logo_fade(1.0, true, dt) - 1.0).abs() < 1e-6);
        assert!(logo_fade(0.0, false, dt).abs() < 1e-6);
    }

    #[test]
    fn a_nonsensical_frame_time_holds_the_crossfade_where_it_was() {
        assert!((logo_fade(0.25, true, f32::NAN) - 0.25).abs() < 1e-6);
        assert!((logo_fade(0.25, true, 0.0) - 0.25).abs() < 1e-6);
        assert!((logo_fade(0.25, false, -1.0) - 0.25).abs() < 1e-6);
        // A poisoned previous value is replaced by the target rather than propagated.
        assert!((logo_fade(f32::NAN, true, 0.1) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_quiet_frame_reports_nothing_at_all() {
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = state(ViewMode::Pro);
        for _ in 0..2 {
            let actions = frame(&ctx, &state, &mut scratch, &mut assets, Vec::new());
            assert!(actions.is_empty(), "an idle bar reported {actions:?}");
        }
        assert!(!scratch.is_dragging_window());
    }

    #[test]
    fn clicking_the_power_button_asks_for_a_power_toggle_and_nothing_else() {
        let state = state(ViewMode::Pro);
        let actions = click_at(&state, Chrome::PRO.power.rect().center());
        assert_eq!(actions, vec![UiAction::TogglePower]);
    }

    #[test]
    fn each_chrome_button_reports_its_own_action() {
        let state = state(ViewMode::Pro);
        for (button, expected) in [
            (Chrome::PRO.menu, UiAction::OpenMenu),
            (Chrome::PRO.flip, UiAction::ToggleView),
            (Chrome::PRO.minimize, UiAction::Minimise),
            (Chrome::PRO.close, UiAction::Close),
        ] {
            let actions = click_at(&state, button.rect().center());
            assert_eq!(actions, vec![expected.clone()], "at {button:?}");
        }
    }

    #[test]
    fn the_lite_flip_button_switches_back_from_its_own_position() {
        // The Lite bar is 508 points wide, so the flip button is at x = 424, not 912.
        let state = state(ViewMode::Lite);
        let actions = click_at(&state, Chrome::LITE.flip.rect().center());
        assert_eq!(actions, vec![UiAction::ToggleView]);
    }

    #[test]
    fn dragging_empty_title_bar_asks_to_move_the_window_exactly_once() {
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = state(ViewMode::Pro);
        // In the long gap between the hamburger and the power button, clear of every hit area.
        let start = pos2(500.0, 28.0);
        let mut actions = Vec::new();

        actions.extend(frame(
            &ctx,
            &state,
            &mut scratch,
            &mut assets,
            vec![Event::PointerMoved(start)],
        ));
        actions.extend(frame(
            &ctx,
            &state,
            &mut scratch,
            &mut assets,
            vec![Event::PointerMoved(start), press(start, true)],
        ));
        for step in 1..=4 {
            let to = start + Vec2::new(12.0 * step as f32, 4.0);
            actions.extend(frame(
                &ctx,
                &state,
                &mut scratch,
                &mut assets,
                vec![Event::PointerMoved(to)],
            ));
        }

        assert_eq!(
            actions,
            vec![UiAction::DragWindow],
            "the drag must be announced once, not once per frame"
        );
        assert!(scratch.is_dragging_window());

        let end = start + Vec2::new(48.0, 4.0);
        frame(
            &ctx,
            &state,
            &mut scratch,
            &mut assets,
            vec![press(end, false)],
        );
        assert!(!scratch.is_dragging_window());
    }

    #[test]
    fn pressing_on_a_button_does_not_start_a_window_drag_instead_of_clicking_it() {
        // The whole point of registering the drag region before the buttons.
        let state = state(ViewMode::Pro);
        let actions = click_at(&state, Chrome::PRO.menu.rect().center());
        assert!(
            !actions.contains(&UiAction::DragWindow),
            "a press on the hamburger leaked into the drag region: {actions:?}"
        );
        assert_eq!(actions, vec![UiAction::OpenMenu]);
    }
}
