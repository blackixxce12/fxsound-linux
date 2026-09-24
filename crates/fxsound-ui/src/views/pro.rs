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
//! preset   (40, 89,  470,  40)   output (530, 89, 225, 40)   input (775, 89, 225, 40)
//! spectrum (40,149,  960, 120)
//! effects  (40,285,  168, 257)   equalizer (224, 285, 776, 257)
//! strip    (40,544,  960,  14)   input edit direction only
//! notice   (440,134, 560, 120)   at most; right-aligned, while there is one
//! ```
//!
//! (`docs/spec/01-window-layout.md` §8.1's table, which is also what [`crate::layout::pro`]
//! returns — with the original's one 470-point playback list split into a list per lane, 0.4.0
//! design §1.4.)
//!
//! ## The effect column
//!
//! `FxAudioControls` is a two-faced card (`docs/spec/03-controls.md` §1): face A is the five effect
//! sliders (`FxEffects`, drawn here), face B the equalizer's own controls — band count, master
//! gain, volume leveling, filter width, balance, Restore Defaults
//! ([`crate::views::equalizer_controls`]). The card's `ControlBackground` fill and the flip button
//! at its top-right corner belong to neither face and are drawn here for both
//! (`FxAudioControls.cpp:80-90`, §5.7, §5.8); which face is up is [`ViewScratch::column_face`].
//!
//! ## What `paint()` does that this does not
//!
//! `FxProView::paint` assigns `setEnabled(...)` to its children from the power state
//! (`FxProView.cpp:112-122`) — a mutation inside a paint routine. In immediate mode the same thing
//! falls out for free: [`UiState::controls_enabled`] is read each frame, and the two device
//! combos and the preset list are the controls deliberately left live with the power off — the
//! preset list as a port departure (0.4.0 audit R7, [`views::preset_combo`]).

use crate::assets::{AssetCache, FxImage};
use crate::layout;
use crate::state::{UiAction, UiResponse, UiState};
use crate::theme::{self, FxColor, Palette};
use crate::views::{
    self, ColumnFace, ViewScratch, at, equalizer_controls, titlebar, window_origin, window_rect,
};
use crate::widgets::icon_button;
use crate::widgets::{EqualizerWidget, FxSlider, IconButton, VisualizerWidget};
use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, Id, Rect, Sense, Ui, Vec2, pos2, vec2,
};
use fxsound_core::i18n::tr;
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

/// The reduction readouts' text, bar height, gap and full scale.
///
/// 24 dB of full scale is past anything a voice preset asks for — the gate's range caps at 14 and
/// the compressor rarely passes 12 — so a bar that is pinned means something has gone wrong rather
/// than that the scale was too short.
pub const METER_FONT_PX: f32 = 11.0;
const METER_BAR_HEIGHT: f32 = 4.0;
const METER_BAR_GAP: f32 = 6.0;
const METER_FULL_SCALE_DB: f32 = 24.0;
/// The strip's slots, left to right: denoiser, noise floor, voice probability, gate, compressor,
/// de-esser (0.4.0 design, §1.4).
pub const STRIP_SLOTS: usize = 6;
/// The shortest bar a metered slot may be left with. Below this a bar is a dot, and a dot does
/// not read as a level.
pub const METER_MIN_BAR: f32 = 16.0;
/// The noise-floor bar spans this range, dBFS: −90 is a studio's silence, −20 a room nobody should
/// be recording in.
pub const FLOOR_BAR_RANGE_DB: (f32, f32) = (-90.0, -20.0);
/// How far the de-esser's built corner has to be from the asked-for one before the strip says so.
pub const DEESSER_MOVED_HZ: f32 = 50.0;
/// What a slot reads while its lane is attached and has nothing to report yet.
const UNMEASURED: &str = "—";

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

    let presets = views::preset_combo(
        ui,
        state,
        palette,
        assets,
        at(origin, layout::pro::preset_combo()),
        &mut response,
    );
    if let Some(tip) = routed_apps_tip(state) {
        let _ = presets.on_hover_text(tip);
    }
    views::lane_combos(ui, state, palette, assets, origin, &mut response);

    VisualizerWidget::new(state, &mut scratch.visualizer).show(
        ui,
        at(origin, layout::pro::visualizer()),
        palette,
    );

    audio_controls(
        ui,
        state,
        &mut scratch.column_face,
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

    if !state.music_effects_apply() {
        input_meters(ui, state, palette, at(origin, layout::pro::input_meters()));
    }

    // Last, so it paints over the visualizer it overlaps and takes the click before it does.
    if let Some(text) = &state.notification {
        notice_bubble(
            ui,
            text,
            palette,
            at(origin, layout::pro::notification()),
            &mut response,
        );
    }

    response
}

/// What the preset list says on hover **(port addition)**: the edit direction's applications that
/// run a preset of their own, one `Battlefield 6 → Gaming` a line, in the order the engine
/// reported them (`docs/0.4.0-apps.md`, "Interface").
///
/// `None` — no tooltip at all — while the lane has none, and while "Hide help tips" is ticked, as
/// for every tip in the window. The names are the applications' and the presets', so there is
/// nothing to translate.
#[must_use]
pub fn routed_apps_tip(state: &UiState) -> Option<String> {
    if state.hide_tooltips {
        return None;
    }
    let lines: Vec<String> = state
        .routed_apps
        .iter()
        .filter(|app| app.direction == state.direction)
        .map(|app| format!("{} → {}", app.name, app.preset))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The notice bubble's geometry and type: `FxNotification` in its in-window, persistent form
/// (`docs/spec/06-dialogs.md` §6).
pub mod notice {
    /// `FxNotification::WIDTH`, the narrowest the bubble gets.
    pub const MIN_WIDTH: f32 = 216.0;
    /// `margin = 40` in persistent mode: twenty points either side of the text.
    pub const MARGIN: f32 = 40.0;
    /// `line_count * 20 + 60`: twenty points a line…
    pub const LINE_PITCH: f32 = 20.0;
    /// …and thirty above and below.
    pub const PADDING_Y: f32 = 30.0;
    /// The original shows three lines at most.
    pub const MAX_LINES: usize = 3;
    /// The rounded rectangle's corner (`FxNotification.cpp:202-213`).
    pub const CORNER_RADIUS: u8 = 16;
    /// `DropShadow` radius.
    pub const SHADOW_BLUR: u8 = 5;
    /// `getSmallFont().withHeight(17.0f)` (`FxNotification.cpp:80`).
    pub const FONT_PX: f32 = 17.0;
}

/// Where a bubble holding `rows` lines of text `text_width` wide goes: its size follows
/// `FxNotification::setMessage` (`docs/spec/06-dialogs.md` §6.3), and it keeps the top-right corner
/// of `max`, the right-aligned position `FxView::showErrorNotification` gives it.
#[must_use]
pub fn notice_rect(max: Rect, text_width: f32, rows: usize) -> Rect {
    let width = (text_width + notice::MARGIN)
        .max(notice::MIN_WIDTH)
        .min(max.width());
    let rows = rows.clamp(1, notice::MAX_LINES) as f32;
    let height = (rows * notice::LINE_PITCH + notice::PADDING_Y * 2.0).min(max.height());
    Rect::from_min_size(pos2(max.right() - width, max.top()), vec2(width, height))
}

/// The notice, drawn the way the original draws its in-window notification: a `DefaultFill`
/// rounded rectangle over a soft shadow, the message centred in `DefaultText`, wrapped to at most
/// three lines. A click takes it down; otherwise the application does, four seconds after it went
/// up. Returns where it was drawn.
fn notice_bubble(
    ui: &Ui,
    text: &str,
    palette: Palette,
    max: Rect,
    response: &mut UiResponse,
) -> Rect {
    let colour = palette.color(FxColor::DefaultText);
    let mut format = egui::TextFormat::simple(
        theme::regular(notice::FONT_PX / icon_button::JUCE_HEIGHT_PER_EM),
        colour,
    );
    format.line_height = Some(notice::LINE_PITCH);
    let mut job = LayoutJob::single_section(text.to_owned(), format);
    job.wrap = TextWrapping {
        max_width: max.width() - notice::MARGIN,
        max_rows: notice::MAX_LINES,
        break_anywhere: false,
        overflow_character: Some('…'),
    };
    job.halign = Align::Center;
    let galley = ui.painter().layout_job(job);
    // `setMessage` goes straight to the widest bubble once any line has had to wrap, rather than
    // fitting the bubble to where the wrap happened to fall.
    let natural = ui
        .painter()
        .layout_no_wrap(
            text.to_owned(),
            theme::regular(notice::FONT_PX / icon_button::JUCE_HEIGHT_PER_EM),
            colour,
        )
        .size()
        .x;
    let text_width = if natural > max.width() - notice::MARGIN {
        max.width()
    } else {
        galley.size().x
    };
    let bubble = notice_rect(max, text_width, galley.rows.len());

    let corner = CornerRadius::same(notice::CORNER_RADIUS);
    let shadow = egui::epaint::Shadow {
        offset: [0, 0],
        blur: notice::SHADOW_BLUR,
        spread: 0,
        color: Color32::from_black_alpha(if palette.is_dark() { 160 } else { 60 }),
    };
    ui.painter().add(shadow.as_shape(bubble, corner));
    ui.painter()
        .rect_filled(bubble, corner, palette.color(FxColor::DefaultFill));
    // A centred job lays its rows out around x = 0, so the galley goes at the bubble's centre line.
    let top = bubble.center().y - galley.size().y / 2.0;
    ui.painter()
        .galley(pos2(bubble.center().x, top), galley, colour);

    let hit = ui.interact(bubble, Id::new("fx_notice_bubble"), Sense::click());
    if hit.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if hit.clicked() {
        response.push(UiAction::DismissNotice);
    }
    bubble
}

/// One slot of the microphone readout strip.
///
/// Three states, not two. A stage can be off; it can be on and working; and it can be asked for
/// and still not running, because the device cannot carry it — a de-esser needs a rate that can
/// hold its crossover, RNNoise exists at 48 kHz and nowhere else, and echo cancellation needs a
/// module the system may not have. Leaving that third state looking like the second is how someone
/// ends up wondering why the sound did not change.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// What the slot says: a name and a reading, or a name and why there is none.
    pub text: String,
    /// How far to fill the slot's bar, `0.0..=1.0`, when it has one.
    pub bar: Option<f32>,
    /// What hovering it adds, if anything.
    pub tip: Option<String>,
}

impl Slot {
    fn off(name: &str) -> Self {
        Self {
            text: format!("{name}  {}", tr("off")),
            bar: None,
            tip: None,
        }
    }

    /// Asked for and not running. The short word goes in the strip; the reason, when there is a
    /// known one, goes in the tip.
    fn unavailable(name: &str, reason: Option<String>) -> Self {
        Self {
            text: format!("{name}  {}", tr("unavailable")),
            bar: None,
            tip: reason,
        }
    }

    /// On, attached, and not delivering: nothing measured to say. A dash, no bar and no tip —
    /// the last buffer's reading would be a number about a microphone that has stopped.
    fn unmeasured(name: &str) -> Self {
        Self {
            text: format!("{name}  {UNMEASURED}"),
            bar: None,
            tip: None,
        }
    }

    /// A gain reduction: positive dB, read as a cut.
    fn reduction(name: &str, reduction_db: f32) -> Self {
        let db = reduction_db.clamp(0.0, 99.9);
        Self {
            text: format!("{name}  {}", signed_db(-db, 1)),
            bar: Some((db / METER_FULL_SCALE_DB).clamp(0.0, 1.0)),
            tip: None,
        }
    }

    /// A stage with the three states and a reduction reading.
    fn stage(
        name: &str,
        on: bool,
        running: bool,
        reduction_db: f32,
        reason: Option<String>,
    ) -> Self {
        match (on, running) {
            (false, _) => Self::off(name),
            (true, false) => Self::unavailable(name, reason),
            (true, true) => Self::reduction(name, reduction_db),
        }
    }
}

/// `−12.5 dB`, with a real minus sign and no `−0`.
fn signed_db(db: f32, decimals: usize) -> String {
    let magnitude = format!("{:.*}", decimals, db.abs());
    let zero = magnitude.chars().all(|c| c == '0' || c == '.');
    if db < 0.0 && !zero {
        format!("−{magnitude} dB")
    } else {
        format!("{magnitude} dB")
    }
}

/// The six slots, left to right, for the input lane's current telemetry.
///
/// Denoise, the noise floor and the voice probability are the denoiser's view of the microphone;
/// the gate, the compressor and the de-esser are the chain's dynamics. Echo cancellation and the
/// de-reverb have no slot of their own: each borrows the gate's or the compressor's slot while that
/// stage is off, and otherwise goes into the Denoise slot's tip, so the strip stays at six and
/// every stage that is on is said somewhere.
///
/// The telemetry is only a reading while the lane runs: the engine writes it from the process
/// callback and leaves it as it was when the callback stops. So every slot keeps the Floor's rule —
/// a lane that is off says `off` in all six, and one that is attached but not delivering shows a
/// dash for each stage that is on, never the last buffer's numbers or a reason that is not the
/// real one.
#[must_use]
pub fn strip_slots(state: &UiState) -> [Slot; STRIP_SLOTS] {
    if !state.input_enabled() {
        return [
            Slot::off(&tr("Denoise")),
            Slot::off(&tr("Floor")),
            Slot::off(&tr("Voice")),
            Slot::off(&tr("Gate")),
            Slot::off(&tr("Compressor")),
            Slot::off(&tr("De-esser")),
        ];
    }
    let idle = !state.input_active;
    // A stage with its three states, or a dash while nothing is measured.
    let stage = |name: &str, on: bool, running: bool, reduction_db: f32, reason| {
        if on && idle {
            Slot::unmeasured(name)
        } else {
            Slot::stage(name, on, running, reduction_db, reason)
        }
    };
    let rate_reason = || Some(tr("unavailable at this rate"));
    let denoise_on = state.denoise_on && state.denoise_level != fxsound_core::DenoiseLevel::Off;
    let mut denoise = stage(
        &tr("Denoise"),
        denoise_on,
        state.denoise_running,
        state.denoise_reduction_db,
        rate_reason(),
    );

    let floor = if !idle && floor_measured(state.noise_floor_db) {
        let db = state.noise_floor_db.max(-120.0);
        let (low, high) = FLOOR_BAR_RANGE_DB;
        Slot {
            text: format!("{}  {}", tr("Floor"), signed_db(db, 0)),
            bar: Some(((db - low) / (high - low)).clamp(0.0, 1.0)),
            tip: None,
        }
    } else {
        // Attached and not measured: the lane is not delivering, or nothing has published a floor
        // yet. A dash, not a number and not a full bar — `0 dB` would be a lie about the room.
        Slot::unmeasured(&tr("Floor"))
    };

    // The voice probability is the denoiser's: no network, no opinion.
    let voice = match (denoise_on, idle, state.denoise_running) {
        (false, _, _) => Slot::off(&tr("Voice")),
        (true, true, _) => Slot::unmeasured(&tr("Voice")),
        (true, false, false) => Slot::unavailable(&tr("Voice"), rate_reason()),
        (true, false, true) => {
            let p = state.voice_probability.clamp(0.0, 1.0);
            Slot {
                text: format!("{}  {:.0} %", tr("Voice"), p * 100.0),
                bar: Some(p),
                tip: None,
            }
        }
    };

    // The canceller's state is the engine's supervisor's, not the process callback's, so it is
    // current whether or not the lane delivers. Not running is only a fault when the engine gave a
    // reason, or when the lane is delivering and the canceller still is not there: before that it
    // is simply not needed yet.
    let echo = || {
        if state.echo_cancel_running {
            Slot {
                text: format!("{}  {}", tr("Echo"), tr("on")),
                bar: None,
                tip: None,
            }
        } else if let Some(trouble) = state.echo_cancel_trouble {
            Slot::unavailable(&tr("Echo"), trouble.reason())
        } else if idle {
            Slot::unmeasured(&tr("Echo"))
        } else {
            Slot::unavailable(&tr("Echo"), None)
        }
    };
    let reverb = || {
        if idle {
            Slot::unmeasured(&tr("Reverb"))
        } else {
            Slot::reduction(&tr("Reverb"), state.dereverb_reduction_db)
        }
    };
    let mut unslotted = Vec::new();

    let gate = if !state.gate_on && state.echo_cancel_on {
        echo()
    } else {
        if state.echo_cancel_on {
            unslotted.push(echo().text);
        }
        stage(
            &tr("Gate"),
            state.gate_on,
            true,
            state.gate_reduction_db,
            None,
        )
    };
    let compressor = if !state.compressor_on && state.dereverb_on {
        reverb()
    } else {
        if state.dereverb_on {
            unslotted.push(reverb().text);
        }
        stage(
            &tr("Compressor"),
            state.compressor_on,
            true,
            state.compressor_reduction_db,
            None,
        )
    };

    let mut deesser = stage(
        &tr("De-esser"),
        state.deesser_on,
        state.deesser_running,
        state.deesser_reduction_db,
        rate_reason(),
    );
    if !idle && state.deesser_on && state.deesser_running && deesser_moved(state) {
        // The adaptive mode lowered the corner for a narrow source; say where it went.
        deesser.text = format!("{}  →{:.1} kHz", deesser.text, state.deesser_hz / 1000.0);
    }

    if !unslotted.is_empty() {
        let mut lines = denoise.tip.take().into_iter().collect::<Vec<_>>();
        lines.extend(unslotted);
        denoise.tip = Some(lines.join("\n"));
    }

    let mut slots = [denoise, floor, voice, gate, compressor, deesser];
    // "Hide help tips" hides every tip, these included. The Echo / Reverb overflow then goes
    // unsaid, which the design allows: it puts that overflow in the tip and nowhere else.
    if state.hide_tooltips {
        for slot in &mut slots {
            slot.tip = None;
        }
    }
    slots
}

/// Whether a noise floor is a measurement. The floor is a running minimum that starts at full
/// scale and falls, and `Meters::default()` carries `0.0`, so a value at or above 0 dBFS — or not
/// a number at all — is "nothing measured yet", never a room as loud as the converter allows.
fn floor_measured(db: f32) -> bool {
    db.is_finite() && db < 0.0
}

/// Whether the de-esser built its corner somewhere other than where it was asked to.
fn deesser_moved(state: &UiState) -> bool {
    state.deesser_hz > 0.0
        && state.deesser_requested_hz > 0.0
        && (state.deesser_requested_hz - state.deesser_hz).abs() >= DEESSER_MOVED_HZ
}

/// Where slot `index` of the strip goes.
#[must_use]
pub fn strip_slot_rect(strip: Rect, index: usize) -> Rect {
    let width = strip.width() / STRIP_SLOTS as f32;
    Rect::from_min_size(
        pos2(strip.left() + width * index as f32, strip.top()),
        vec2(width, strip.height()),
    )
}

/// The rectangles one slot's text and bar take, given the text's natural width: the text as laid
/// out, the bar from a gap after it to a gap before the next slot — never shorter than
/// [`METER_MIN_BAR`], the text giving way (it is elided) rather than the bar.
#[must_use]
pub fn strip_slot_parts(slot: Rect, text_width: f32, metered: bool) -> (Rect, Option<Rect>) {
    let room = if metered {
        slot.width() - METER_BAR_GAP * 2.0 - METER_MIN_BAR
    } else {
        slot.width() - METER_BAR_GAP
    };
    let text = Rect::from_min_size(slot.left_top(), vec2(text_width.min(room), slot.height()));
    let bar = metered.then(|| {
        Rect::from_min_max(
            pos2(
                text.right() + METER_BAR_GAP,
                slot.center().y - METER_BAR_HEIGHT / 2.0,
            ),
            pos2(
                slot.right() - METER_BAR_GAP,
                slot.center().y + METER_BAR_HEIGHT / 2.0,
            ),
        )
    });
    (text, bar)
}

/// The microphone chain's readouts.
///
/// A voice chain that is working is a chain that is *changing* something, and none of its stages
/// has a control the user can watch: the gate opens and closes on its own, the compressor rides
/// the delivery, the de-esser fires on a syllable. Without this the only feedback is the sound
/// itself, which is exactly the feedback someone setting up a microphone does not yet trust.
///
/// Drawn only in the input edit direction, in space the original leaves as padding.
fn input_meters(ui: &Ui, state: &UiState, palette: Palette, strip: Rect) {
    let label_colour = palette.color(FxColor::DefaultText);
    let bar_colour = palette.color(FxColor::HighlightedText);
    let font = caption_font(METER_FONT_PX);

    for (index, slot) in strip_slots(state).into_iter().enumerate() {
        let area = strip_slot_rect(strip, index);
        let natural = ui
            .painter()
            .layout_no_wrap(slot.text.clone(), font.clone(), label_colour)
            .size()
            .x;
        let (text_rect, bar) = strip_slot_parts(area, natural, slot.bar.is_some());
        // Elided rather than overlapping the next slot, for the languages whose words are longer
        // than the slot was measured for.
        let mut job = LayoutJob::single_section(
            slot.text.clone(),
            egui::TextFormat::simple(font.clone(), label_colour),
        );
        job.wrap = TextWrapping::truncate_at_width(text_rect.width());
        let galley = ui.painter().layout_job(job);
        ui.painter().galley(
            pos2(text_rect.left(), area.center().y - galley.size().y / 2.0),
            galley,
            label_colour,
        );

        if let (Some(fill), Some(bar)) = (slot.bar, bar) {
            // A bar as well as a number: a number that changes twenty times a second is not
            // something anyone reads. The unlit track needs a colour of its own — the panel's
            // background is what it is drawn on top of, so painting it there makes an empty meter
            // invisible, which is exactly what the first build did.
            ui.painter()
                .rect_filled(bar, 1.0, palette.color_alpha(FxColor::DefaultText, 0.15));
            if fill > 0.0 {
                let mut lit = bar;
                lit.set_width(bar.width() * fill.clamp(0.0, 1.0));
                ui.painter().rect_filled(lit, 1.0, bar_colour);
            }
        }

        if let Some(tip) = slot.tip {
            let _ = ui
                .interact(area, Id::new("fx_strip_slot").with(index), Sense::hover())
                .on_hover_text(
                    egui::RichText::new(tip)
                        .font(caption_font(METER_FONT_PX + 2.0))
                        .color(label_colour),
                );
        }
    }
}

/// The effect column: the card, whichever face is up, and the flip button over both
/// (`FxAudioControls`, `FxAudioControls.cpp:26-90`).
fn audio_controls(
    ui: &mut Ui,
    state: &UiState,
    face: &mut ColumnFace,
    palette: Palette,
    assets: &mut AssetCache,
    column: Rect,
    response: &mut UiResponse,
) {
    // `FxAudioControls::paint`: the whole card in `ControlBackground`, radius 8 (§5.8) — the same
    // card the equalizer beside it sits on.
    ui.painter().rect_filled(
        column,
        CornerRadius::same(layout::PANEL_CORNER_RADIUS as u8),
        palette.color(FxColor::ControlBackground),
    );

    match *face {
        ColumnFace::Effects => effect_column(ui, state, palette, assets, column, response),
        ColumnFace::EqualizerControls => {
            equalizer_controls::show(ui, state, palette, assets, column, response);
        }
    }

    // Last, so it is on top of either face. A child of the card, so the power switch disables it
    // with everything else (`FxProView.cpp:112-122`). It has no tooltip in the original, and the
    // small glyph is the only way to the master gain, the leveling, the width, the balance and
    // the band count (0.4.0 audit #27): the tip names the face a click turns to.
    let enabled = state.controls_enabled();
    let tip = tr(match *face {
        ColumnFace::Effects => "Equalizer settings",
        ColumnFace::EqualizerControls => "Effects",
    });
    let flip = IconButton::new(FxImage::FlipButton)
        .hover(FxImage::FlipButtonHover)
        .enabled(enabled)
        .opacity(if enabled {
            1.0
        } else {
            equalizer_controls::DISABLED_BUTTON_OPACITY
        })
        .tooltip(&tip)
        .hide_tooltips(state.hide_tooltips)
        .show(
            ui,
            equalizer_controls::flip_button(column),
            palette,
            assets,
            "fx_column_flip",
        );
    if flip.clicked() {
        *face = face.flipped();
        // The face this frame painted is the old one; the next shows the new one at once rather
        // than whenever something else next asks for a frame.
        ui.ctx().request_repaint();
    }
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
    // Two different reasons to be grey, and they are not the same reason. The power switch turns
    // the controls off; a microphone means these five controls are not *for* this chain at all.
    let applies = state.music_effects_apply();
    let enabled = state.controls_enabled() && applies;
    let caption_colour = palette.color(FxColor::DefaultText);
    // At a disabled label's half alpha while the power is off, as face B's readouts are.
    let value_colour = if enabled {
        palette.color(FxColor::HighlightedText)
    } else {
        palette.color_alpha(
            FxColor::HighlightedText,
            equalizer_controls::DISABLED_TEXT_ALPHA,
        )
    };

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
        // 0…10 in whole steps (`FxAudioControls.cpp:113`), and with Shift one stored value at a
        // time (0.4.0 audit #14): the eleven positions stand over 128 values a preset can store.
        // A right-click switches the effect off, as it puts a level back to its neutral value on
        // the other face: the original gives only `FxAudioSlider` and `FxBalanceSlider` the reset
        // (`docs/spec/03-controls.md` §3.5), and the port gives it these five too (0.4.0 audit R9).
        let steps = StoredSteps(effect);
        let slider = FxSlider::new(&mut value, 0.0, scale::SLIDER_MAX, 1.0)
            .fine_steps(&steps)
            .default_value(0.0)
            .reset_on_secondary_click(true)
            .enabled(enabled)
            .show(ui, rect, palette, assets, effect.key());
        let changed = slider.changed();
        // The five help tips (`FxAudioControls.cpp:157-161`), with the right-click under them,
        // cleared while the user has ticked "Hide help tips for audio controls" (`:169-176`). On a
        // microphone the tip is replaced rather than dropped: the question a greyed control raises
        // is "why", and the answer has to be somewhere the user is already looking.
        if !state.hide_tooltips {
            let _ = if applies {
                slider.on_hover_text(crate::widgets::slider::with_reset_tip(Some(&tr(
                    effect.tooltip()
                ))))
            } else {
                slider.on_hover_text(tr(MICROPHONE_INERT_TIP))
            };
        }
        if changed {
            response.push(UiAction::SetEffect(effect, value));
        }

        // `showValue(show)` is `show && isEnabled()` (`FxAudioControls.cpp:208-211`), and since
        // v2.0 `show` is unconditionally true so touch users can read the value
        // (`FxProView.cpp:70`). With the power off the original's values go with `isEnabled()`,
        // which is an accident of a stale state rather than a rule — "values always visible since
        // version 2.0" (`FxProView.cpp:56-73`) — so here they stay, at half alpha, and say what
        // switching back on will play (0.4.0 audit #42). On a microphone they are the playback
        // chain's and not this one's, and the slider says so instead.
        //
        // A value between two positions — General's Surround is stored as 20, between positions 1
        // and 2 — shows with its decimal, "1.6", rather than as the position ("2") that would
        // save as something else (0.4.0 audit #14).
        if applies {
            let t = value / scale::SLIDER_MAX;
            let label = effects::value_label_rect(rect, t);
            ui.painter().text(
                pos2(label.left() + effects::LABEL_BORDER_LEFT, label.center().y),
                Align2::LEFT_CENTER,
                scale::slider_label_for(effect, value),
                caption_font(VALUE_FONT_PX),
                value_colour,
            );
        }
    }

    // A hover tip is not enough on its own: nobody hovers a control they have already decided is
    // broken. The sixth row's worth of space below the five sliders is where the original leaves
    // padding, and it is drawn on only in the direction the original does not have.
    if !applies {
        ui.painter().text(
            pos2(
                column.left() + effects::X_MARGIN + crate::widgets::slider::THUMB_RADIUS,
                effects::row_top(column, Effect::COUNT),
            ),
            Align2::LEFT_TOP,
            tr(MICROPHONE_INERT_CAPTION),
            caption_font(CAPTION_FONT_PX),
            palette.color(FxColor::DefaultText),
        );
    }
}

/// The values an effect's slider stands at between its whole positions: every value a preset can
/// store, one Shift-step apart ([`scale::stored_step_for`]).
struct StoredSteps(Effect);

impl crate::widgets::slider::FineSteps for StoredSteps {
    fn step(&self, value: f32, up: bool) -> f32 {
        scale::stored_step_for(self.0, value, up)
    }

    fn nearest(&self, value: f32) -> f32 {
        scale::nearest_stored_position_for(self.0, value)
    }
}

/// Why the five effect sliders are grey while a microphone is selected.
///
/// English keys, like every other string the port added that the Windows catalogues never had:
/// [`tr`] falls back to the key itself, so an untranslated build reads correctly rather than
/// showing a placeholder.
pub const MICROPHONE_INERT_CAPTION: &str = "Not used on a microphone";
pub const MICROPHONE_INERT_TIP: &str = "These five belong to the playback chain. A microphone runs the voice chain instead: \
     high-pass, gate, equalizer, de-esser, compressor and limiter.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PresetEntry;
    use crate::views::testing::{Harness, outline_of, text_below, texts};
    use crate::widgets::{combo, slider};
    use egui::{Event, PointerButton, Pos2, RawInput};
    use fxsound_core::{AudioDevice, DeviceDirection, ThemeMode};

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
                form_factor: "speaker".into(),
            }],
            selected_output: Some(0),
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
        // lost silently. Tessellating a real frame is the cheapest way to keep that honest, in each
        // shape the window can take: editing the output, editing the microphone with its strip, and
        // either with the longest notice there is.
        let window = Rect::from_min_size(Pos2::ZERO, views::window_size(ViewMode::Pro));
        let long = "A notice long enough to need every one of the three lines the bubble allows \
                    and then some more, so that the elision is exercised as well as the wrapping \
                    and nobody has to wonder whether a fourth line would hang out of the bubble.";
        for (label, state) in [
            ("output", state()),
            ("microphone", strip_state()),
            (
                "output with a notice",
                UiState {
                    notification: Some(long.to_owned()),
                    ..state()
                },
            ),
            (
                "microphone with a notice",
                UiState {
                    notification: Some(long.to_owned()),
                    ..strip_state()
                },
            ),
        ] {
            for mode in [ThemeMode::Dark, ThemeMode::Light] {
                let mut harness = Harness::new(mode);
                let shapes = harness.settle(&state);
                let painted = views::testing::painted_bounds(&shapes);
                assert!(painted.is_positive(), "{label}: the window painted nothing");
                // A point of slack for the feathering epaint puts around every antialiased edge.
                assert!(
                    window.expand(1.0).contains_rect(painted),
                    "{label}, {mode:?}: {painted:?} spills out of {window:?}"
                );
                // …and it really did cover the window, rather than passing by painting almost
                // nothing.
                assert!(
                    painted.width() > window.width() - 2.0
                        && painted.height() > window.height() - 2.0,
                    "{label}: only {painted:?} of {window:?} was painted"
                );
            }
        }
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

    /// General's Surround: stored as 20, between positions 1 (13) and 2 (25) — the audit's
    /// scenario for #14.
    fn surround_between_positions() -> UiState {
        let mut state = state();
        state.effects[Effect::Surround as usize] = scale::midi_to_slider_for(Effect::Surround, 20);
        state
    }

    fn thumb_of(state: &UiState, effect: Effect) -> Pos2 {
        let rect = effects::slider_rect(column(), effect as usize);
        let track = slider::track_rect(rect);
        let t = state.effect(effect) / scale::SLIDER_MAX;
        Pos2::new(track.left() + track.width() * t, rect.center().y)
    }

    #[test]
    fn a_value_between_positions_is_shown_with_its_decimal_and_a_position_without() {
        // 0.4.0 audit #14: the readout used to round 1.57 to "2", which saves as 25.
        let mut harness = Harness::new(ThemeMode::Dark);
        let shapes = harness.settle(&surround_between_positions());
        let shown: Vec<String> = texts(&shapes).into_iter().map(|(text, ..)| text).collect();
        assert!(shown.iter().any(|t| t == "1.6"), "{shown:?}");
        assert!(!shown.iter().any(|t| t == "2"), "{shown:?}");
        // The whole positions of the other four still read as whole numbers.
        for whole in ["3", "5", "4", "8"] {
            assert!(shown.iter().any(|t| t == whole), "{whole} in {shown:?}");
        }
    }

    #[test]
    fn a_press_on_the_thumb_without_moving_leaves_a_value_between_positions_alone() {
        // 0.4.0 audit #14: one touch snapped the stored 20 to a whole position and the preset
        // was saved as 25.
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = surround_between_positions();
        harness.settle(&state);
        let actions = harness.click(&state, thumb_of(&state, Effect::Surround));
        assert!(actions.is_empty(), "a touch moved the slider: {actions:?}");
    }

    #[test]
    fn a_drag_off_the_thumb_still_lands_on_whole_positions() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = surround_between_positions();
        harness.settle(&state);
        let from = thumb_of(&state, Effect::Surround);
        let to = from + egui::vec2(34.0, 0.0);
        let press = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(from)],
            vec![Event::PointerMoved(from), press(from, true)],
            vec![Event::PointerMoved(to)],
            vec![press(to, false)],
        ] {
            actions.extend(harness.frame(&state, events).0);
        }
        let Some(UiAction::SetEffect(Effect::Surround, value)) = actions.last() else {
            panic!("the drag did nothing: {actions:?}");
        };
        assert_eq!(*value, value.round(), "{value}");
    }

    #[test]
    fn shift_and_an_arrow_step_one_stored_value_where_an_arrow_steps_a_position() {
        // 0.4.0 audit #14: the fine step reaches every value a preset can store, the Windows
        // presets' 20 included, and the plain arrow keeps its whole positions.
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let state = surround_between_positions();
        let key = |modifiers| Event::Key {
            key: egui::Key::ArrowRight,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let focus = || {
            ctx.memory_mut(|m| {
                m.request_focus(egui::Id::new("fx_slider").with(Effect::Surround.key()));
            });
        };
        frame(&ctx, &state, &mut scratch, &mut assets, Vec::new());
        focus();
        frame(&ctx, &state, &mut scratch, &mut assets, Vec::new());
        // Shift held: the modifiers of the frame, as egui-winit reports them, and of the key.
        let mut actions = Vec::new();
        let input = raw_input(vec![
            Event::ModifiersChanged(egui::Modifiers::SHIFT),
            key(egui::Modifiers::SHIFT),
        ]);
        ctx.run_ui(input, |ui| {
            actions = show(
                ui,
                &state,
                &mut scratch,
                Palette::new(ThemeMode::Dark),
                &mut assets,
            )
            .actions;
        })
        .drop_without_applying_deltas();
        let Some(UiAction::SetEffect(Effect::Surround, value)) = actions.first() else {
            panic!("Shift+Right did nothing: {actions:?}");
        };
        assert_eq!(scale::slider_to_midi_for(Effect::Surround, *value), 21);

        focus();
        frame(
            &ctx,
            &state,
            &mut scratch,
            &mut assets,
            vec![Event::ModifiersChanged(egui::Modifiers::NONE)],
        );
        let actions = frame(
            &ctx,
            &state,
            &mut scratch,
            &mut assets,
            vec![key(egui::Modifiers::NONE)],
        );
        assert_eq!(
            actions.first(),
            Some(&UiAction::SetEffect(Effect::Surround, 2.0)),
            "a plain Right arrow from 1.6 stops at the next position, 2, and does not skip it"
        );

        let left = Event::Key {
            key: egui::Key::ArrowLeft,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        focus();
        frame(&ctx, &state, &mut scratch, &mut assets, Vec::new());
        let actions = frame(&ctx, &state, &mut scratch, &mut assets, vec![left]);
        assert_eq!(
            actions.first(),
            Some(&UiAction::SetEffect(Effect::Surround, 1.0)),
            "a plain Left arrow from 1.6 stops at the position below it, 1"
        );
    }

    #[test]
    fn shift_and_one_wheel_notch_step_one_stored_value_where_a_notch_steps_a_position() {
        // 0.4.0 audit #14 and #48: the wheel reads raw notches now, and Shift still turns one of
        // them into one stored value, not a position and not a run of them.
        let wheel = |modifiers| Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, 1.0),
            phase: egui::TouchPhase::Move,
            modifiers,
        };
        let state = surround_between_positions();
        let over = effects::slider_rect(column(), Effect::Surround as usize).center();

        let mut harness = Harness::new(ThemeMode::Dark);
        harness.frame(&state, vec![Event::PointerMoved(over)]);
        harness.settle(&state);
        let mut actions = harness
            .frame(
                &state,
                vec![
                    Event::ModifiersChanged(egui::Modifiers::SHIFT),
                    wheel(egui::Modifiers::SHIFT),
                ],
            )
            .0;
        for _ in 0..30 {
            actions.extend(harness.frame(&state, Vec::new()).0);
        }
        let stepped: Vec<u8> = actions
            .iter()
            .filter_map(|action| match action {
                UiAction::SetEffect(Effect::Surround, value) => {
                    Some(scale::slider_to_midi_for(Effect::Surround, *value))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            stepped,
            vec![21],
            "Shift and one notch up from 20: {actions:?}"
        );

        let mut harness = Harness::new(ThemeMode::Dark);
        harness.frame(&state, vec![Event::PointerMoved(over)]);
        harness.settle(&state);
        let actions = harness.frame(&state, vec![wheel(egui::Modifiers::NONE)]).0;
        assert_eq!(
            actions.first(),
            Some(&UiAction::SetEffect(Effect::Surround, 2.0)),
            "a plain notch up from 1.6 stops at the next position, 2"
        );
    }

    /// Press and release `button` at `pos`, every frame's actions reported.
    fn press(
        harness: &mut Harness,
        state: &UiState,
        pos: Pos2,
        button: PointerButton,
    ) -> Vec<UiAction> {
        let event = |pressed| Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(pos)],
            vec![Event::PointerMoved(pos), event(true)],
            vec![event(false)],
        ] {
            actions.extend(harness.frame(state, events).0);
        }
        actions
    }

    #[test]
    fn a_right_click_on_an_effect_switches_it_off_without_moving_it_to_the_pointer_first() {
        // 0.4.0 audit R9: the original gives the reset to the level sliders only.
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = state();
        harness.settle(&state);
        for effect in Effect::ALL {
            let track = slider::track_rect(effects::slider_rect(column(), effect as usize));
            let at = pos2(track.right() - 2.0, track.center().y);
            let actions = press(&mut harness, &state, at, PointerButton::Secondary);
            assert_eq!(
                actions,
                vec![UiAction::SetEffect(effect, 0.0)],
                "{effect:?}"
            );
        }
        // An effect already off has nothing to reset.
        let mut off = state.clone();
        off.effects[Effect::Bass as usize] = 0.0;
        let track = slider::track_rect(effects::slider_rect(column(), Effect::Bass as usize));
        assert!(press(&mut harness, &off, track.center(), PointerButton::Secondary).is_empty());
    }

    #[test]
    fn an_effects_tip_names_the_right_click_under_its_own() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let rect = effects::slider_rect(column(), Effect::Bass as usize);
        let shown = harness.rest(&state(), rect.center());
        let tip = shown
            .iter()
            .find(|text| text.starts_with("Boosts low end"))
            .unwrap_or_else(|| panic!("no tip in {shown:?}"));
        assert!(tip.ends_with(slider::RESET_TIP), "{tip:?}");
        // On a microphone the tip is the reason instead, and there is nothing to reset.
        let mut harness = Harness::new(ThemeMode::Dark);
        let shown = harness.rest(&microphone_state(), rect.center());
        assert!(shown.iter().any(|t| t == MICROPHONE_INERT_TIP), "{shown:?}");
        assert!(
            !shown.iter().any(|t| t.contains(slider::RESET_TIP)),
            "{shown:?}"
        );
    }

    #[test]
    fn with_the_power_off_the_effect_values_stay_on_screen_at_half_alpha() {
        // 0.4.0 audit #42: the original hides them with `isEnabled()`, against its own "values
        // always visible since version 2.0".
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let palette = Palette::new(mode);
            let mut harness = Harness::new(mode);
            let shapes = harness.settle(&UiState {
                power: false,
                ..state()
            });
            let greyed = palette.color_alpha(
                FxColor::HighlightedText,
                equalizer_controls::DISABLED_TEXT_ALPHA,
            );
            for (index, value) in ["3", "5", "7", "4", "8"].into_iter().enumerate() {
                let slider = effects::slider_rect(column(), index);
                let found: Vec<_> = texts(&shapes)
                    .into_iter()
                    .filter(|(text, rect, _)| text == value && slider.contains(rect.center()))
                    .collect();
                assert_eq!(found.len(), 1, "{mode:?} row {index}: {found:?}");
                assert_eq!(found[0].2, greyed, "{mode:?} row {index}");
            }
            // With the power on they are at full strength.
            let shapes = harness.settle(&state());
            let (_, _, colour) = texts(&shapes)
                .into_iter()
                .find(|(text, rect, _)| {
                    text == "3" && effects::slider_rect(column(), 0).contains(rect.center())
                })
                .expect("Clarity's value");
            assert_eq!(colour, palette.color(FxColor::HighlightedText), "{mode:?}");
        }
    }

    #[test]
    fn the_card_flip_says_which_face_it_turns_to() {
        // 0.4.0 audit #27: the one way to the master gain, the leveling, the width, the balance
        // and the band count had no word on it.
        let state = state();
        let flip = equalizer_controls::flip_button(column()).center();
        let mut harness = Harness::new(ThemeMode::Dark);
        assert!(
            harness
                .rest(&state, flip)
                .contains(&"Equalizer settings".to_owned())
        );
        let mut harness = Harness::new(ThemeMode::Dark);
        harness.scratch.column_face = ColumnFace::EqualizerControls;
        assert!(harness.rest(&state, flip).contains(&"Effects".to_owned()));
        let mut harness = Harness::new(ThemeMode::Dark);
        let hidden = UiState {
            hide_tooltips: true,
            ..state
        };
        assert!(
            !harness
                .rest(&hidden, flip)
                .contains(&"Equalizer settings".to_owned())
        );
    }

    #[test]
    fn with_the_power_off_the_band_gains_stay_on_screen_greyed_and_readable() {
        // 0.4.0 audit #42, the equalizer's half; greyed to 3:1 on the panel (review FA).
        use crate::widgets::equalizer::{EqLayout, gain_label, gain_label_colour};
        let mut state = state();
        state.eq_bands[3].boost_db = 5.0;
        let panel = layout::pro::equalizer();
        let layout = EqLayout::new(state.eq_bands.len());
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let palette = Palette::new(mode);
            let mut harness = Harness::new(mode);
            let off = UiState {
                power: false,
                ..state.clone()
            };
            let shapes = harness.settle(&off);
            for (band, eq_band) in off.eq_bands.iter().enumerate() {
                let label = layout
                    .gain_label_rect(band, eq_band.boost_db)
                    .translate(panel.min.to_vec2());
                let wanted = gain_label(eq_band.boost_db);
                let found: Vec<_> = texts(&shapes)
                    .into_iter()
                    .filter(|(text, rect, _)| {
                        *text == wanted && (rect.center().x - label.center().x).abs() < 1.0
                    })
                    .collect();
                assert_eq!(found.len(), 1, "{mode:?} band {band}: {found:?}");
                assert_eq!(
                    found[0].2,
                    gain_label_colour(palette, false),
                    "{mode:?} band {band}"
                );
            }
            assert_ne!(
                gain_label_colour(palette, false),
                gain_label_colour(palette, true),
                "{mode:?}: drawn as disabled"
            );
        }
        assert_eq!(
            gain_label_colour(Palette::new(ThemeMode::Dark), false),
            Palette::new(ThemeMode::Dark).color_alpha(FxColor::DefaultText, 0.5)
        );
        assert_eq!(
            gain_label_colour(Palette::new(ThemeMode::Light), false),
            egui::Color32::from_gray(crate::theme::LIGHT_GREY_LIMIT)
        );
    }

    #[test]
    fn every_band_tip_names_the_right_click_and_the_solo_and_at_ten_bands_says_what_the_band_is_first()
     {
        use crate::widgets::equalizer::{BAND_TOOLTIPS, EqLayout, SOLO_TIP};
        let gestures = format!("{}\n{SOLO_TIP}", slider::RESET_TIP);
        let panel = layout::pro::equalizer();
        for count in [10, 31] {
            let mut state = state();
            state.eq_bands = (0..count)
                .map(|band| fxsound_core::EqBand::new(30.0 + 500.0 * band as f32, 0.0))
                .collect();
            let hit = EqLayout::new(count)
                .gain_hit_rect(2)
                .translate(panel.min.to_vec2());
            let mut harness = Harness::new(ThemeMode::Dark);
            let shown = harness.rest(&state, hit.center());
            let tip = shown
                .iter()
                .find(|text| text.ends_with(&gestures))
                .unwrap_or_else(|| panic!("{count} bands: {shown:?}"));
            if count == 10 {
                assert!(tip.starts_with(BAND_TOOLTIPS[2]), "{tip:?}");
            } else {
                assert_eq!(*tip, gestures);
            }
        }
    }

    #[test]
    fn a_bypassed_equalizer_is_grey_that_can_be_seen_in_the_light_palette() {
        // 0.4.0 audit #24: the light palette's curve fill greyed to white.
        let palette = Palette::new(ThemeMode::Light);
        let panel_colour = palette.color(FxColor::ControlBackground);
        let mut state = state();
        state.eq_bands[4].boost_db = 8.0;
        state.eq_on = false;
        let mut harness = Harness::new(ThemeMode::Light);
        let shapes = harness.settle(&state);
        let eq = layout::pro::equalizer();
        let meshes: Vec<_> = shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Mesh(mesh)
                    if mesh.texture_id == egui::TextureId::default()
                        && mesh.vertices.iter().all(|v| eq.contains(v.pos)) =>
                {
                    Some(mesh.clone())
                }
                _ => None,
            })
            .collect();
        let fill = meshes.first().expect("the curve's fill");
        // The fill fades from 0.34 alpha to none, and a faint vertex's straight colour is only
        // known to a few levels; the ones that show are the grey `Palette::greyed` gives, which
        // reads at 3:1 on the panel, where the original's was white.
        let limit = crate::theme::LIGHT_GREY_LIMIT;
        let seen: Vec<_> = fill
            .vertices
            .iter()
            .map(|vertex| vertex.color.to_srgba_unmultiplied())
            .filter(|[.., a]| *a >= 40)
            .collect();
        assert!(!seen.is_empty(), "{:?}", fill.vertices);
        for [r, g, b, _] in seen {
            assert!(r == g && g == b, "{r} {g} {b}");
            assert!(r.abs_diff(limit) <= 6, "{r:#x}");
        }
        let grey = egui::Color32::from_gray(limit);
        assert!(crate::theme::contrast_ratio(grey, panel_colour) >= 3.0);
    }

    /// The same window with a microphone selected.
    fn microphone_state() -> UiState {
        let mut state = state();
        state.devices = vec![AudioDevice {
            id: 7,
            name: "alsa_input.usb-fifine.analog-stereo".to_owned(),
            description: "fifine Microphone".to_owned(),
            is_default: true,
            direction: fxsound_core::DeviceDirection::Input,
            form_factor: "microphone".into(),
        }];
        state.direction = fxsound_core::DeviceDirection::Input;
        state
    }

    #[test]
    fn the_effect_sliders_do_nothing_on_a_microphone_and_do_not_pretend_otherwise() {
        // They belong to the music chain. The point of this test is the *silence*: clicking a
        // slider that is drawn live but changes nothing audible is the defect, so the assertion is
        // that no action leaves the view at all.
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let microphone = microphone_state();

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
            actions.extend(frame(&ctx, &microphone, &mut scratch, &mut assets, events));
        }
        assert!(
            actions.is_empty(),
            "a microphone's effect slider still moved: {actions:?}"
        );

        // And the same click on a playback device does move it, so this is the direction talking
        // and not a broken slider.
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
            actions.extend(frame(&ctx, &state(), &mut scratch, &mut assets, events));
        }
        assert_eq!(actions, vec![UiAction::SetEffect(Effect::Fidelity, 5.0)]);
    }

    #[test]
    fn the_reason_has_a_place_to_be_written_inside_the_column() {
        // A hover tip alone would not do: nobody hovers a control they have already written off.
        // So the reason is painted where a sixth slider row would start — inside the column, clear
        // of the fifth slider, in space the original leaves as padding.
        let column = column();
        let y = effects::row_top(column, Effect::COUNT);
        assert!(
            y + CAPTION_FONT_PX <= column.bottom(),
            "the caption at {y} does not fit the column ending at {}",
            column.bottom()
        );
        assert!(
            y > effects::slider_rect(column, Effect::COUNT - 1).bottom(),
            "the caption overlaps the last slider"
        );
    }

    #[test]
    fn the_reduction_meters_are_drawn_only_where_there_is_a_chain_to_meter() {
        // Geometry, not pixels: the strip must sit in the panel's own padding, so that nothing
        // that exists in the output direction moves by a point.
        let strip = layout::pro::input_meters();
        let panel = layout::pro::panel();
        let controls = layout::pro::audio_controls();
        let eq = layout::pro::equalizer();
        assert!(
            strip.top() >= controls.bottom(),
            "the strip overlaps the sliders"
        );
        assert!(
            strip.top() >= eq.bottom(),
            "the strip overlaps the equalizer"
        );
        assert!(
            strip.bottom() <= panel.bottom(),
            "the strip leaves the panel"
        );
        assert!(strip.height() >= 10.0, "no room to draw a meter in");
        assert_eq!(strip.left(), controls.left());
        assert_eq!(strip.right(), eq.right());
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

    // ---- two lanes ---------------------------------------------------------------------------

    fn device(id: u32, name: &str, direction: DeviceDirection) -> AudioDevice {
        AudioDevice {
            id,
            name: format!("node.{name}"),
            description: name.to_owned(),
            is_default: false,
            direction,
            form_factor: String::new(),
        }
    }

    /// Two speakers and two microphones, the output lane on the second speaker, the input lane off.
    fn lanes_state() -> UiState {
        UiState {
            devices: vec![
                device(1, "Speakers", DeviceDirection::Output),
                device(2, "Headphones", DeviceDirection::Output),
                device(3, "Microphone", DeviceDirection::Input),
                device(4, "Webcam", DeviceDirection::Input),
            ],
            selected_output: Some(1),
            selected_input: None,
            ..state()
        }
    }

    /// Open the list at `combo` and pick the row reading `label`, collecting every action.
    fn pick(harness: &mut Harness, state: &UiState, combo: Rect, label: &str) -> Vec<UiAction> {
        let (mut actions, picked) = open_then_pick(harness, state, state, combo, label);
        actions.extend(picked);
        actions
    }

    /// The same, with the state the window is in when the menu is used — `after` — separate from
    /// the one it was opened in, as the application's answer to the opening click would make it.
    fn open_then_pick(
        harness: &mut Harness,
        before: &UiState,
        after: &UiState,
        combo: Rect,
        label: &str,
    ) -> (Vec<UiAction>, Vec<UiAction>) {
        let mut opened = harness.click(before, combo.center());
        // The sizing pass, then a painted menu.
        harness.frame(after, Vec::new());
        let (more, shapes) = harness.frame(after, Vec::new());
        opened.extend(more);
        let rows = text_below(&shapes, label, combo.bottom() - 1.0);
        assert_eq!(rows.len(), 1, "{label:?} is listed {} times", rows.len());
        (opened, harness.click(after, rows[0].center()))
    }

    #[test]
    fn the_output_list_lists_off_and_the_speakers_and_no_microphone() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = lanes_state();
        let combo = layout::pro::output_combo();
        harness.click(&state, combo.center());
        harness.frame(&state, Vec::new());
        let (_, shapes) = harness.frame(&state, Vec::new());
        let below = combo.bottom() - 1.0;
        assert_eq!(text_below(&shapes, "Off", below).len(), 1);
        assert_eq!(text_below(&shapes, "Speakers", below).len(), 1);
        assert_eq!(text_below(&shapes, "Headphones", below).len(), 1);
        assert!(text_below(&shapes, "Microphone", below).is_empty());
        assert!(text_below(&shapes, "Webcam", below).is_empty());
    }

    #[test]
    fn picking_off_in_the_input_list_detaches_the_input() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = UiState {
            selected_input: Some(2),
            direction: DeviceDirection::Input,
            ..lanes_state()
        };
        let actions = pick(&mut harness, &state, layout::pro::input_combo(), "Off");
        assert_eq!(actions, vec![UiAction::DetachInput]);
    }

    #[test]
    fn picking_off_in_the_output_list_detaches_the_output() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = lanes_state();
        let actions = pick(&mut harness, &state, layout::pro::output_combo(), "Off");
        assert_eq!(actions, vec![UiAction::DetachOutput]);
    }

    #[test]
    fn opening_the_other_lanes_list_makes_it_the_edit_direction() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = lanes_state();
        let actions = harness.click(&state, layout::pro::input_combo().center());
        assert_eq!(
            actions,
            vec![UiAction::SetEditDirection(DeviceDirection::Input)]
        );
        // The edited lane's own list opens without saying anything.
        let mut harness = Harness::new(ThemeMode::Dark);
        let actions = harness.click(&state, layout::pro::output_combo().center());
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn picking_a_microphone_edits_the_input_and_attaches_it() {
        // As the application runs it: the opening click moves the edit direction at once, and the
        // pick that follows only has the device left to say.
        let mut harness = Harness::new(ThemeMode::Dark);
        let before = lanes_state();
        let after = UiState {
            direction: DeviceDirection::Input,
            ..lanes_state()
        };
        let (opened, picked) = open_then_pick(
            &mut harness,
            &before,
            &after,
            layout::pro::input_combo(),
            "Webcam",
        );
        assert_eq!(
            opened,
            vec![UiAction::SetEditDirection(DeviceDirection::Input)]
        );
        assert_eq!(picked, vec![UiAction::SelectInput(3)]);

        // And if nothing answered the click, the pick still says both, direction first.
        let mut harness = Harness::new(ThemeMode::Dark);
        let (_, picked) = open_then_pick(
            &mut harness,
            &before,
            &before,
            layout::pro::input_combo(),
            "Webcam",
        );
        assert_eq!(
            picked,
            vec![
                UiAction::SetEditDirection(DeviceDirection::Input),
                UiAction::SelectInput(3)
            ]
        );
    }

    #[test]
    fn picking_a_speaker_in_the_edited_list_only_selects_it() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = lanes_state();
        let actions = pick(
            &mut harness,
            &state,
            layout::pro::output_combo(),
            "Speakers",
        );
        assert_eq!(actions, vec![UiAction::SelectOutput(0)]);
    }

    #[test]
    fn only_the_edit_directions_list_carries_the_accent() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let palette = Palette::new(mode);
            let accent = combo::box_outline_colour(palette, None, false, true);
            let plain = combo::box_outline_colour(palette, None, false, false);
            for (edited, other) in [
                (DeviceDirection::Output, DeviceDirection::Input),
                (DeviceDirection::Input, DeviceDirection::Output),
            ] {
                let mut harness = Harness::new(mode);
                let state = UiState {
                    direction: edited,
                    selected_input: Some(2),
                    ..lanes_state()
                };
                let shapes = harness.settle(&state);
                assert_eq!(
                    outline_of(&shapes, layout::pro::device_combo(edited)),
                    Some(accent),
                    "{mode:?}: the {edited:?} list is edited"
                );
                assert_eq!(
                    outline_of(&shapes, layout::pro::device_combo(other)),
                    Some(plain),
                    "{mode:?}: the {other:?} list is not"
                );
                assert_eq!(
                    outline_of(&shapes, layout::pro::preset_combo()),
                    Some(plain),
                    "the preset list is never accented"
                );
            }
        }
    }

    #[test]
    fn a_detached_lane_shows_off_dimmed_and_an_attached_one_its_device() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = lanes_state();
        let shapes = harness.settle(&state);
        let input = layout::pro::input_combo();
        let output = layout::pro::output_combo();
        let inside = |rect: Rect| {
            texts(&shapes)
                .into_iter()
                .filter(move |(_, at, _)| rect.contains(at.center()))
                .collect::<Vec<_>>()
        };
        let off = inside(input);
        assert_eq!(off.len(), 1);
        assert_eq!(off[0].0, "Off");
        assert_eq!(
            off[0].2,
            harness
                .palette
                .color_alpha(FxColor::DefaultText, combo::PLACEHOLDER_ALPHA)
        );
        let on = inside(output);
        assert_eq!(on.len(), 1);
        assert_eq!(on[0].0, "Headphones");
    }

    #[test]
    fn both_device_lists_stay_live_with_the_power_off() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = UiState {
            power: false,
            ..lanes_state()
        };
        let actions = pick(
            &mut harness,
            &state,
            layout::pro::input_combo(),
            "Microphone",
        );
        assert!(actions.contains(&UiAction::SelectInput(2)), "{actions:?}");
    }

    // ---- the readout strip -------------------------------------------------------------------

    /// The microphone being edited, with every stage on and running.
    fn strip_state() -> UiState {
        UiState {
            direction: DeviceDirection::Input,
            selected_input: Some(2),
            input_active: true,
            denoise_on: true,
            denoise_running: true,
            gate_on: true,
            compressor_on: true,
            deesser_on: true,
            deesser_running: true,
            voice_probability: 0.93,
            denoise_reduction_db: 18.2,
            noise_floor_db: -42.0,
            gate_reduction_db: 3.1,
            compressor_reduction_db: 2.0,
            deesser_reduction_db: 1.0,
            deesser_hz: 5_500.0,
            deesser_requested_hz: 5_500.0,
            ..lanes_state()
        }
    }

    fn slot_texts(state: &UiState) -> Vec<String> {
        strip_slots(state)
            .into_iter()
            .map(|slot| slot.text)
            .collect()
    }

    #[test]
    fn the_strip_reads_denoise_floor_voice_gate_compressor_de_esser() {
        assert_eq!(
            slot_texts(&strip_state()),
            [
                "Denoise  −18.2 dB",
                "Floor  −42 dB",
                "Voice  93 %",
                "Gate  −3.1 dB",
                "Compressor  −2.0 dB",
                "De-esser  −1.0 dB",
            ]
        );
        assert!(
            strip_slots(&strip_state())
                .iter()
                .all(|slot| slot.bar.is_some())
        );
    }

    #[test]
    fn the_six_slots_tile_the_strip_exactly() {
        let strip = layout::pro::input_meters();
        assert_eq!(
            strip,
            Rect::from_min_size(pos2(40.0, 544.0), vec2(960.0, 14.0))
        );
        for index in 0..STRIP_SLOTS {
            let slot = strip_slot_rect(strip, index);
            assert!((slot.width() - 160.0).abs() < 1e-4);
            assert!(strip.contains_rect(slot));
            if index > 0 {
                assert!((slot.left() - strip_slot_rect(strip, index - 1).right()).abs() < 1e-4);
            }
        }
        assert!((strip_slot_rect(strip, STRIP_SLOTS - 1).right() - strip.right()).abs() < 1e-4);
    }

    #[test]
    fn a_stage_nobody_switched_on_says_off_and_draws_no_bar() {
        let state = UiState {
            denoise_on: false,
            gate_on: false,
            compressor_on: false,
            deesser_on: false,
            selected_input: None,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert_eq!(
            slot_texts(&state),
            [
                "Denoise  off",
                "Floor  off",
                "Voice  off",
                "Gate  off",
                "Compressor  off",
                "De-esser  off",
            ]
        );
        assert!(slots.iter().all(|slot| slot.bar.is_none()));
    }

    #[test]
    fn a_suppression_level_of_off_is_a_denoiser_that_is_off() {
        let state = UiState {
            denoise_level: fxsound_core::DenoiseLevel::Off,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert_eq!(slots[0].text, "Denoise  off");
        assert_eq!(slots[2].text, "Voice  off");
    }

    #[test]
    fn a_stage_asked_for_and_not_running_says_so_and_tells_why_on_hover() {
        let state = UiState {
            denoise_running: false,
            deesser_running: false,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert_eq!(slots[0].text, "Denoise  unavailable");
        assert_eq!(slots[2].text, "Voice  unavailable");
        assert_eq!(slots[5].text, "De-esser  unavailable");
        for index in [0, 2, 5] {
            assert!(slots[index].bar.is_none());
            assert_eq!(
                slots[index].tip.as_deref(),
                Some("unavailable at this rate"),
                "slot {index}"
            );
        }
    }

    #[test]
    fn echo_cancellation_takes_the_gate_slot_only_while_the_gate_is_off() {
        let state = UiState {
            gate_on: false,
            echo_cancel_on: true,
            echo_cancel_running: true,
            ..strip_state()
        };
        assert_eq!(strip_slots(&state)[3].text, "Echo  on");
        let state = UiState {
            echo_cancel_running: false,
            ..state
        };
        assert_eq!(strip_slots(&state)[3].text, "Echo  unavailable");

        // With the gate on, the gate keeps its slot and the echo canceller goes in the tip.
        let state = UiState {
            echo_cancel_on: true,
            echo_cancel_running: true,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert_eq!(slots[3].text, "Gate  −3.1 dB");
        assert_eq!(slots[0].tip.as_deref(), Some("Echo  on"));
    }

    #[test]
    fn the_de_reverb_takes_the_compressor_slot_only_while_the_compressor_is_off() {
        let state = UiState {
            compressor_on: false,
            dereverb_on: true,
            dereverb_reduction_db: 2.5,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert_eq!(slots[4].text, "Reverb  −2.5 dB");
        assert!(slots[4].bar.is_some());
        assert!(slots[0].tip.is_none());

        let state = UiState {
            compressor_on: true,
            ..state
        };
        let slots = strip_slots(&state);
        assert_eq!(slots[4].text, "Compressor  −2.0 dB");
        assert_eq!(slots[0].tip.as_deref(), Some("Reverb  −2.5 dB"));
    }

    #[test]
    fn both_extra_stages_share_the_denoise_tip_when_neither_has_a_slot() {
        let state = UiState {
            echo_cancel_on: true,
            echo_cancel_running: false,
            dereverb_on: true,
            dereverb_reduction_db: 1.0,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert_eq!(slots.len(), STRIP_SLOTS, "the strip stays at six");
        assert_eq!(
            slots[0].tip.as_deref(),
            Some("Echo  unavailable\nReverb  −1.0 dB")
        );
    }

    #[test]
    fn the_de_esser_names_its_corner_only_when_the_adaptive_mode_moved_it() {
        let moved = UiState {
            deesser_hz: 4_000.0,
            ..strip_state()
        };
        assert_eq!(strip_slots(&moved)[5].text, "De-esser  −1.0 dB  →4.0 kHz");
        for hz in [5_500.0, 5_480.0, 0.0] {
            let state = UiState {
                deesser_hz: hz,
                ..strip_state()
            };
            assert_eq!(strip_slots(&state)[5].text, "De-esser  −1.0 dB", "{hz} Hz");
        }
        // Not known what was asked for: say nothing rather than guess.
        let unknown = UiState {
            deesser_requested_hz: 0.0,
            ..moved
        };
        assert_eq!(strip_slots(&unknown)[5].text, "De-esser  −1.0 dB");
    }

    #[test]
    fn the_floor_bar_spans_ninety_to_twenty_below_full_scale() {
        let bar = |db: f32| {
            strip_slots(&UiState {
                noise_floor_db: db,
                ..strip_state()
            })[1]
                .bar
                .expect("a floor bar")
        };
        assert!(bar(-100.0).abs() < 1e-6);
        assert!(bar(-90.0).abs() < 1e-6);
        assert!((bar(-55.0) - 0.5).abs() < 1e-6);
        assert!((bar(-20.0) - 1.0).abs() < 1e-6);
        // Pinned above the top of the range, short of full scale: 0 dBFS itself is no reading.
        assert!((bar(-0.5) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_detached_microphone_says_off_in_every_slot_whatever_its_last_readings_were() {
        // After a call the user picks Off: the telemetry is the last buffer's, and nothing clears
        // it. With stages switched on and readings left over, every slot still says off.
        let state = UiState {
            selected_input: None,
            input_active: false,
            echo_cancel_on: true,
            dereverb_on: true,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert_eq!(
            slot_texts(&state),
            [
                "Denoise  off",
                "Floor  off",
                "Voice  off",
                "Gate  off",
                "Compressor  off",
                "De-esser  off",
            ]
        );
        assert!(
            slots
                .iter()
                .all(|slot| slot.bar.is_none() && slot.tip.is_none())
        );
    }

    #[test]
    fn a_first_run_with_the_microphone_off_names_no_reason_that_is_not_the_real_one() {
        // A shipped voice preset with the denoiser and the de-esser on, nothing ever run: no
        // "unavailable at this rate", no empty gate bar.
        let state = UiState {
            selected_input: None,
            input_active: false,
            denoise_running: false,
            deesser_running: false,
            voice_probability: 0.0,
            gate_reduction_db: 0.0,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert!(
            slots.iter().all(|slot| slot.text.ends_with("off")),
            "{slots:?}"
        );
        assert!(slots.iter().all(|slot| slot.tip.is_none()));
    }

    #[test]
    fn an_attached_microphone_that_is_not_delivering_shows_a_dash_for_each_stage_that_is_on() {
        let state = UiState {
            input_active: false,
            compressor_on: false,
            ..strip_state()
        };
        let slots = strip_slots(&state);
        assert_eq!(
            slot_texts(&state),
            [
                "Denoise  —",
                "Floor  —",
                "Voice  —",
                "Gate  —",
                "Compressor  off",
                "De-esser  —",
            ],
            "the last buffer's numbers are not a reading"
        );
        assert!(slots.iter().all(|slot| slot.bar.is_none()));
        assert!(
            slots.iter().all(|slot| slot.tip.is_none()),
            "no reason to give"
        );
    }

    #[test]
    fn the_borrowed_echo_and_reverb_slots_keep_the_same_rules() {
        let idle = UiState {
            input_active: false,
            gate_on: false,
            compressor_on: false,
            echo_cancel_on: true,
            dereverb_on: true,
            dereverb_reduction_db: 4.0,
            ..strip_state()
        };
        let slots = strip_slots(&idle);
        assert_eq!(slots[3].text, "Echo  —", "not needed yet is no fault");
        assert_eq!(slots[4].text, "Reverb  —");
        assert!(slots[4].bar.is_none());

        // Running, the canceller is on whether or not the lane delivers.
        let running = UiState {
            echo_cancel_running: true,
            ..idle.clone()
        };
        assert_eq!(strip_slots(&running)[3].text, "Echo  on");

        // A reason from the engine is a fault whatever the lane is doing, and says why.
        let trouble = UiState {
            echo_cancel_trouble: Some(crate::state::EchoCancelTrouble::NotLoaded),
            ..idle
        };
        let slot = &strip_slots(&trouble)[3];
        assert_eq!(slot.text, "Echo  unavailable");
        assert_eq!(
            slot.tip.as_deref(),
            Some("the echo canceller could not be loaded")
        );
    }

    #[test]
    fn a_floor_nothing_has_measured_reads_a_dash_and_draws_no_bar() {
        // `Meters::default()` carries a floor of 0.0, and the running minimum starts there: until
        // it has come down, there is no floor to report, and a full bar would say the room is as
        // loud as the converter goes.
        for db in [0.0, 3.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let slot = &strip_slots(&UiState {
                noise_floor_db: db,
                ..strip_state()
            })[1];
            assert_eq!(slot.text, "Floor  —", "{db}");
            assert!(slot.bar.is_none(), "{db}");
            assert!(slot.tip.is_none(), "{db}");
        }
    }

    #[test]
    fn a_floor_on_a_microphone_that_is_not_delivering_reads_a_dash_not_its_last_value() {
        let slot = &strip_slots(&UiState {
            input_active: false,
            ..strip_state()
        })[1];
        assert_eq!(slot.text, "Floor  —");
        assert!(slot.bar.is_none());
    }

    #[test]
    fn a_floor_on_a_detached_microphone_still_says_off() {
        let slot = &strip_slots(&UiState {
            selected_input: None,
            input_active: false,
            noise_floor_db: 0.0,
            ..strip_state()
        })[1];
        assert_eq!(slot.text, "Floor  off");
        assert!(slot.bar.is_none());
    }

    #[test]
    fn a_measured_floor_just_under_full_scale_is_still_a_reading() {
        let slot = &strip_slots(&UiState {
            noise_floor_db: -0.4,
            ..strip_state()
        })[1];
        assert_eq!(slot.text, "Floor  0 dB");
        assert!(slot.bar.is_some());
    }

    #[test]
    fn with_help_tips_hidden_no_slot_offers_a_tip() {
        // The rate reason and the Echo / Reverb overflow are both tips, and "Hide help tips"
        // means every tip.
        let noisy = UiState {
            denoise_running: false,
            deesser_running: false,
            echo_cancel_on: true,
            dereverb_on: true,
            ..strip_state()
        };
        assert!(
            strip_slots(&noisy).iter().any(|slot| slot.tip.is_some()),
            "the state has tips to hide"
        );
        let hidden = UiState {
            hide_tooltips: true,
            ..noisy.clone()
        };
        for (index, slot) in strip_slots(&hidden).iter().enumerate() {
            assert!(slot.tip.is_none(), "slot {index}: {:?}", slot.tip);
        }
        // Only the tips go: what the strip itself says is unchanged.
        assert_eq!(slot_texts(&hidden), slot_texts(&noisy));
    }

    #[test]
    fn hiding_help_tips_leaves_no_hover_target_on_the_strip() {
        let ctx = test_context();
        let mut scratch = ViewScratch::new();
        let mut assets = AssetCache::new();
        let slot = Id::new("fx_strip_slot").with(0);

        let shown = UiState {
            denoise_running: false,
            ..strip_state()
        };
        let _ = frame(&ctx, &shown, &mut scratch, &mut assets, Vec::new());
        assert!(
            ctx.read_response(slot).is_some(),
            "an unavailable denoiser offers its reason on hover"
        );

        let hidden = UiState {
            hide_tooltips: true,
            ..shown
        };
        // Two passes: egui answers from the pass before the last as well.
        for _ in 0..2 {
            let _ = frame(&ctx, &hidden, &mut scratch, &mut assets, Vec::new());
        }
        assert!(
            ctx.read_response(slot).is_none(),
            "with help tips hidden the slot is not a hover target"
        );
    }

    #[test]
    fn the_readings_never_print_a_minus_zero_or_run_past_their_width() {
        assert_eq!(signed_db(-0.01, 1), "0.0 dB");
        assert_eq!(signed_db(-0.4, 0), "0 dB");
        assert_eq!(signed_db(0.0, 0), "0 dB");
        assert_eq!(signed_db(-42.0, 0), "−42 dB");
        let state = UiState {
            gate_reduction_db: 250.0,
            noise_floor_db: -400.0,
            voice_probability: 7.0,
            ..strip_state()
        };
        let texts = slot_texts(&state);
        assert_eq!(texts[3], "Gate  −99.9 dB");
        assert_eq!(texts[1], "Floor  −120 dB");
        assert_eq!(texts[2], "Voice  100 %");
    }

    /// The widest thing each slot can say in English.
    fn widest_states() -> Vec<UiState> {
        let loud = UiState {
            denoise_reduction_db: 99.9,
            noise_floor_db: -120.0,
            voice_probability: 1.0,
            gate_reduction_db: 99.9,
            compressor_reduction_db: 99.9,
            deesser_reduction_db: 99.9,
            deesser_hz: 3_900.0,
            ..strip_state()
        };
        let unavailable = UiState {
            denoise_running: false,
            deesser_running: false,
            echo_cancel_on: true,
            gate_on: false,
            ..strip_state()
        };
        let reverb = UiState {
            compressor_on: false,
            dereverb_on: true,
            dereverb_reduction_db: 99.9,
            ..strip_state()
        };
        vec![strip_state(), loud, unavailable, reverb]
    }

    #[test]
    fn every_slot_fits_its_text_and_a_bar_at_every_value() {
        // The strip is 960 points for six slots. A slot that ran into the next would make two
        // readings one unreadable one, so in English every reading must fit whole beside a bar at
        // least `METER_MIN_BAR` long; other languages are elided rather than allowed to spill.
        let ctx = test_context();
        let strip = layout::pro::input_meters();
        ctx.run_ui(raw_input(Vec::new()), |ui| {
            for state in widest_states() {
                for (index, slot) in strip_slots(&state).into_iter().enumerate() {
                    let area = strip_slot_rect(strip, index);
                    let galley = ui.painter().layout_no_wrap(
                        slot.text.clone(),
                        caption_font(METER_FONT_PX),
                        egui::Color32::PLACEHOLDER,
                    );
                    let width = galley.size().x;
                    let (text, bar) = strip_slot_parts(area, width, slot.bar.is_some());
                    assert!(
                        (text.width() - width).abs() < 1e-3,
                        "{:?} is {width} points and was cut to {}",
                        slot.text,
                        text.width()
                    );
                    assert!(galley.size().y <= area.height() + 0.5, "{:?}", slot.text);
                    assert!(area.contains_rect(text), "{:?}", slot.text);
                    if let Some(bar) = bar {
                        assert!(
                            bar.width() >= METER_MIN_BAR - 1e-3,
                            "{:?} leaves a {} point bar",
                            slot.text,
                            bar.width()
                        );
                        assert!(area.contains_rect(bar));
                        assert!(bar.left() >= text.right());
                    }
                }
            }
        })
        .drop_without_applying_deltas();
    }

    #[test]
    fn a_reading_too_long_for_its_slot_is_cut_rather_than_spilled() {
        let slot = strip_slot_rect(layout::pro::input_meters(), 0);
        let (text, bar) = strip_slot_parts(slot, 1_000.0, true);
        let bar = bar.expect("metered");
        assert!(slot.contains_rect(text) && slot.contains_rect(bar));
        assert!((bar.width() - METER_MIN_BAR).abs() < 1e-3);
        let (text, bar) = strip_slot_parts(slot, 1_000.0, false);
        assert!(bar.is_none());
        assert!(slot.contains_rect(text));
    }

    /// Every reading the strip can make in English, from the widest states and the ones where a
    /// stage says `off`, `on` or `unavailable` instead of a number, with whether it is metered.
    fn every_english_reading() -> Vec<(String, bool)> {
        let off = UiState {
            denoise_on: false,
            gate_on: false,
            compressor_on: false,
            deesser_on: false,
            ..strip_state()
        };
        let detached = UiState {
            selected_input: None,
            ..off.clone()
        };
        let echo_running = UiState {
            gate_on: false,
            echo_cancel_on: true,
            echo_cancel_running: true,
            ..strip_state()
        };
        let mut states = widest_states();
        states.extend([off, detached, echo_running]);
        states
            .iter()
            .flat_map(strip_slots)
            .map(|slot| (slot.text, slot.bar.is_some()))
            .collect()
    }

    #[test]
    fn every_language_fits_every_reading_whole_beside_its_bar() {
        // The English test above, in every table: a reading is a stage's name, two spaces and a
        // number or a status word, so each English reading is rebuilt with its name and its word
        // translated and its number as it is. The strip elides what still does not fit, but a
        // stage named "Rauschunterdrü…" is the translation's doing, not the strip's, so the
        // translations were chosen short enough that nothing is.
        use fxsound_core::i18n::{Catalogue, LANGUAGES};
        let readings = every_english_reading();
        assert!(
            readings.iter().any(|(text, _)| text.ends_with("  off"))
                && readings.iter().any(|(text, _)| text.ends_with("  on"))
                && readings
                    .iter()
                    .any(|(text, _)| text.ends_with("  unavailable")),
            "{readings:?}"
        );
        let ctx = test_context();
        let slot = strip_slot_rect(layout::pro::input_meters(), 0);
        let mut problems = Vec::new();
        ctx.run_ui(raw_input(Vec::new()), |ui| {
            for language in &LANGUAGES[1..] {
                let table = Catalogue::for_language(language);
                let translate = |key: &str| table.get(key).unwrap_or(key).to_owned();
                for (english, metered) in &readings {
                    let (name, value) = english.split_once("  ").expect("a name and a value");
                    let value = match value {
                        "off" | "on" | "unavailable" => translate(value),
                        number => number.to_owned(),
                    };
                    let text = format!("{}  {value}", translate(name));
                    let width = ui
                        .painter()
                        .layout_no_wrap(
                            text.clone(),
                            caption_font(METER_FONT_PX),
                            egui::Color32::PLACEHOLDER,
                        )
                        .size()
                        .x;
                    let (room, _) = strip_slot_parts(slot, width, *metered);
                    if room.width() + 1e-3 < width {
                        problems.push(format!(
                            "{}: {text:?} is {width:.0} points in {:.0}",
                            language.code,
                            room.width()
                        ));
                    }
                }
            }
        })
        .drop_without_applying_deltas();
        problems.dedup();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn every_language_fits_the_microphone_note_in_the_room_the_captions_above_it_have() {
        // The note is painted with no width of its own, so a translation longer than the five
        // captions' 160 points runs into the equalizer panel and is cut there: German's
        // "Bei einem Mikrofon ohne Wirkung" lost its last letters that way.
        use fxsound_core::i18n::{Catalogue, LANGUAGES};
        let ctx = test_context();
        let column = layout::pro::audio_controls();
        let room = effects::caption_rect(column, Effect::COUNT).width();
        let mut problems = Vec::new();
        ctx.run_ui(raw_input(Vec::new()), |ui| {
            for language in &LANGUAGES[1..] {
                let table = Catalogue::for_language(language);
                let text = table
                    .get(MICROPHONE_INERT_CAPTION)
                    .unwrap_or(MICROPHONE_INERT_CAPTION);
                let width = ui
                    .painter()
                    .layout_no_wrap(
                        text.to_owned(),
                        caption_font(CAPTION_FONT_PX),
                        egui::Color32::PLACEHOLDER,
                    )
                    .size()
                    .x;
                if width > room {
                    problems.push(format!(
                        "{}: {text:?} is {width:.0} points in {room:.0}",
                        language.code
                    ));
                }
            }
        })
        .drop_without_applying_deltas();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn the_strip_is_painted_in_the_input_direction_and_not_in_the_output_one() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let strip = layout::pro::input_meters();
        let in_strip = |shapes: &[egui::epaint::ClippedShape]| {
            texts(shapes)
                .into_iter()
                .filter(|(_, rect, _)| strip.contains(rect.center()))
                .map(|(text, _, _)| text)
                .collect::<Vec<_>>()
        };
        let shapes = harness.settle(&strip_state());
        assert_eq!(in_strip(&shapes), slot_texts(&strip_state()));
        let mut harness = Harness::new(ThemeMode::Dark);
        let shapes = harness.settle(&lanes_state());
        assert!(in_strip(&shapes).is_empty());
    }

    // ---- the notice bubble -------------------------------------------------------------------

    /// The bubble's fill: the `DefaultFill` rectangle painted inside the notice's rect.
    fn bubble(shapes: &[egui::epaint::ClippedShape], palette: Palette) -> Option<Rect> {
        let max = layout::pro::notification();
        shapes.iter().find_map(|clipped| match &clipped.shape {
            egui::Shape::Rect(shape)
                if shape.fill == palette.color(FxColor::DefaultFill)
                    && max.contains_rect(shape.rect) =>
            {
                Some(shape.rect)
            }
            _ => None,
        })
    }

    #[test]
    fn a_notice_is_drawn_as_a_bubble_under_the_device_lists() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let mut harness = Harness::new(mode);
            let state = UiState {
                notification: Some("Preset: Rock".to_owned()),
                ..state()
            };
            let shapes = harness.settle(&state);
            let rect = bubble(&shapes, harness.palette).expect("a bubble");
            let max = layout::pro::notification();
            assert_eq!(
                rect.right(),
                max.right(),
                "right-aligned to the device lists"
            );
            assert_eq!(rect.top(), max.top());
            assert_eq!(
                rect.width(),
                notice::MIN_WIDTH,
                "a short message, the narrowest bubble"
            );
            assert_eq!(rect.height(), 80.0, "one line: 20 + 60");
            let text = texts(&shapes)
                .into_iter()
                .find(|(text, _, _)| text == "Preset: Rock")
                .expect("the message is painted");
            assert!(
                rect.contains_rect(text.1),
                "{:?} is outside {rect:?}",
                text.1
            );
            assert!(
                (text.1.center().x - rect.center().x).abs() < 1.0,
                "the message is centred"
            );
            assert_eq!(text.2, harness.palette.color(FxColor::DefaultText));
        }
    }

    #[test]
    fn no_notice_draws_no_bubble() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let shapes = harness.settle(&state());
        assert!(bubble(&shapes, harness.palette).is_none());
    }

    #[test]
    fn a_long_notice_wraps_to_three_lines_inside_the_originals_largest_bubble() {
        let mut harness = Harness::new(ThemeMode::Light);
        let long = "word ".repeat(200);
        let state = UiState {
            notification: Some(long),
            ..state()
        };
        let shapes = harness.settle(&state);
        let rect = bubble(&shapes, harness.palette).expect("a bubble");
        assert_eq!(rect, layout::pro::notification(), "560 x 120 at most");
        let text = texts(&shapes)
            .into_iter()
            .find(|(text, _, _)| text.starts_with("word"))
            .expect("the message");
        assert!(
            rect.contains_rect(text.1),
            "{:?} is outside {rect:?}",
            text.1
        );
    }

    #[test]
    fn the_bubble_grows_with_its_text_and_keeps_its_top_right_corner() {
        let max = layout::pro::notification();
        let short = notice_rect(max, 50.0, 1);
        assert_eq!(short.width(), notice::MIN_WIDTH);
        let mid = notice_rect(max, 300.0, 2);
        assert_eq!(mid.width(), 340.0);
        assert_eq!(mid.height(), 100.0);
        let huge = notice_rect(max, 5_000.0, 9);
        assert_eq!(huge, max);
        for rect in [short, mid, huge] {
            assert_eq!(rect.right_top(), max.right_top());
            assert!(max.contains_rect(rect));
        }
    }

    #[test]
    fn clicking_the_bubble_takes_the_notice_down() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = UiState {
            notification: Some("Preset: Rock".to_owned()),
            ..state()
        };
        let shapes = harness.settle(&state);
        let rect = bubble(&shapes, harness.palette).expect("a bubble");
        let actions = harness.click(&state, rect.center());
        assert_eq!(actions, vec![UiAction::DismissNotice]);
    }

    #[test]
    fn the_bubble_is_painted_over_the_visualizer_it_overlaps() {
        let mut harness = Harness::new(ThemeMode::Dark);
        let state = UiState {
            notification: Some("Preset: Rock".to_owned()),
            audio_active: true,
            ..state()
        };
        let shapes = harness.settle(&state);
        let palette = harness.palette;
        let fill = shapes
            .iter()
            .position(|clipped| {
                matches!(&clipped.shape, egui::Shape::Rect(shape)
                if shape.fill == palette.color(FxColor::DefaultFill)
                    && layout::pro::notification().contains_rect(shape.rect))
            })
            .expect("a bubble");
        let visualizer = layout::pro::visualizer();
        let last_underneath = shapes
            .iter()
            .rposition(|clipped| {
                let bounds = clipped.shape.visual_bounding_rect();
                bounds.intersects(visualizer)
                    && !layout::pro::notification()
                        .expand(6.0)
                        .contains_rect(bounds)
            })
            .unwrap_or(0);
        assert!(
            fill > last_underneath,
            "something was painted over the bubble"
        );
    }

    // ---- the preset list's tip: applications with a preset of their own ----------------------

    fn routed(direction: DeviceDirection, name: &str, preset: &str) -> crate::state::RoutedApp {
        crate::state::RoutedApp {
            direction,
            name: name.to_owned(),
            preset: preset.to_owned(),
        }
    }

    /// Battlefield 6 and Brave routed on the speakers, Discord on the microphone.
    fn routed_state() -> UiState {
        UiState {
            routed_apps: vec![
                routed(DeviceDirection::Output, "Battlefield 6", "Gaming"),
                routed(DeviceDirection::Input, "Discord", "Headset"),
                routed(DeviceDirection::Output, "Brave", "Volume Boost"),
            ],
            ..lanes_state()
        }
    }

    #[test]
    fn the_preset_lists_tip_names_the_edit_directions_routed_applications_only() {
        let state = routed_state();
        assert_eq!(
            routed_apps_tip(&state).as_deref(),
            Some("Battlefield 6 → Gaming\nBrave → Volume Boost")
        );
        let state = UiState {
            direction: DeviceDirection::Input,
            ..routed_state()
        };
        assert_eq!(
            routed_apps_tip(&state).as_deref(),
            Some("Discord → Headset")
        );
    }

    #[test]
    fn there_is_no_tip_without_a_routed_application_or_with_help_tips_hidden() {
        assert_eq!(routed_apps_tip(&lanes_state()), None);
        // Routed on the other lane only.
        let state = UiState {
            routed_apps: vec![routed(DeviceDirection::Input, "Discord", "Headset")],
            ..lanes_state()
        };
        assert_eq!(routed_apps_tip(&state), None);
        let state = UiState {
            hide_tooltips: true,
            ..routed_state()
        };
        assert_eq!(routed_apps_tip(&state), None);
    }

    /// The tip's lines on screen after the pointer has rested on the preset list for a second.
    fn tip_shown(state: &UiState) -> Vec<String> {
        let mut harness = Harness::new(ThemeMode::Dark);
        let over = layout::pro::preset_combo().center();
        harness.frame(state, vec![Event::PointerMoved(over)]);
        // Sixty quiet frames of a sixtieth each: past egui's half-second tooltip delay.
        let mut shapes = Vec::new();
        for _ in 0..60 {
            shapes = harness.frame(state, Vec::new()).1;
        }
        texts(&shapes)
            .into_iter()
            .map(|(text, _, _)| text)
            .filter(|text| text.contains('→'))
            .collect()
    }

    #[test]
    fn resting_on_the_preset_list_shows_the_tip_and_only_when_there_is_one() {
        let shown = tip_shown(&routed_state());
        assert_eq!(shown.len(), 1, "{shown:?}");
        assert!(shown[0].contains("Battlefield 6 → Gaming"), "{shown:?}");
        assert!(shown[0].contains("Brave → Volume Boost"), "{shown:?}");
        assert!(!shown[0].contains("Discord"), "{shown:?}");
        assert!(tip_shown(&lanes_state()).is_empty());
        assert!(
            tip_shown(&UiState {
                hide_tooltips: true,
                ..routed_state()
            })
            .is_empty()
        );
    }

    #[test]
    fn the_lite_window_says_nothing_about_routed_applications() {
        let state = UiState {
            view: ViewMode::Lite,
            ..routed_state()
        };
        let mut harness = Harness::new(ThemeMode::Dark);
        let over = layout::lite::preset_combo().center();
        harness.frame(&state, vec![Event::PointerMoved(over)]);
        let mut shapes = Vec::new();
        for _ in 0..60 {
            shapes = harness.frame(&state, Vec::new()).1;
        }
        assert!(
            texts(&shapes)
                .iter()
                .all(|(text, _, _)| !text.contains('→')),
            "the design keeps the tip to the Pro window"
        );
    }
}
