//! The microphone calibration wizard (0.4.0 design §8).
//!
//! Three short measurements — silence, normal speech, loud speech — then a table of what was
//! heard and the voice settings it suggests, with Apply and Retry. Hosted inside the main window
//! the way the Import dialog is: a titled [`DialogChrome`], a content size per phase, and a pane
//! over a dimmed backdrop.
//!
//! ## Pure view
//!
//! [`CalibrationDialog`] borrows a [`CalibrationView`] — the phase, the countdown, the live input
//! level and, at the end, the result — and returns a [`DialogResponse<CalibrationAction>`]. Timing
//! the phases, resetting the capture statistics on entry to each, reading them back on exit and
//! turning them into a recommendation is the application's state machine; nothing here sees a
//! meter, a clock or a preset store. Whether there is such a state machine and a microphone on
//! the input lane for it to read is the host's to say ([`CalibrationView::can_measure`]); until it
//! does, Start and Retry are disabled; the host wakes a lane that is attached but idle itself.
//! Every string the result carries (the recommended settings, the reason a run failed) arrives
//! already translated, because the application is what knows which numbers they hold.
//!
//! ## What the ✕ means
//!
//! While the wizard is still running — the introduction, the three measurements, the analysis —
//! the ✕ and Escape are [`CalibrationAction::Cancel`]: stop listening and leave without applying
//! anything. Once there is a result or a failure they are [`CalibrationAction::Close`], which is
//! the same "leave without applying" said of a wizard that has already stopped.

use super::{
    DialogChrome, DialogResponse, TextButton, draw_truncated, draw_wrapped, normal_font,
    small_font, whole_db,
};
use crate::assets::AssetCache;
use crate::theme::{FxColor, Palette};
use egui::{Align2, CornerRadius, Id, Key, Mesh, Rect, Shape, Ui, Vec2, pos2, vec2};
use fxsound_core::i18n::tr;

// =============================================================================================
// The view model
// =============================================================================================

/// Where the wizard is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CalibrationPhase {
    /// What is about to happen, and Start.
    #[default]
    Intro,
    /// Three seconds of silence: the noise floor.
    Silence,
    /// Five seconds of normal speech: the speaking level and its crest.
    Speech,
    /// Two seconds of loud speech: whether the input clips.
    Loud,
    /// The measurements are in and the recommendation is being worked out.
    Analysing,
    /// The table and Apply.
    Result,
    /// Something went wrong — no signal, the microphone went away — and why.
    Failed,
}

impl CalibrationPhase {
    /// In the order the wizard goes through them.
    pub const ALL: [Self; 7] = [
        Self::Intro,
        Self::Silence,
        Self::Speech,
        Self::Loud,
        Self::Analysing,
        Self::Result,
        Self::Failed,
    ];

    /// How long the application listens in this phase: three seconds of silence, five of speech,
    /// two of loud speech (§8). Zero for the phases that are not timed.
    #[must_use]
    pub const fn seconds(self) -> f32 {
        match self {
            Self::Silence => 3.0,
            Self::Speech => 5.0,
            Self::Loud => 2.0,
            Self::Intro | Self::Analysing | Self::Result | Self::Failed => 0.0,
        }
    }

    /// Whether the microphone is being listened to: the three timed phases, which show the
    /// countdown and the live level.
    #[must_use]
    pub const fn is_measuring(self) -> bool {
        matches!(self, Self::Silence | Self::Speech | Self::Loud)
    }

    /// Whether the wizard is still going, i.e. there is neither a result nor a failure yet.
    /// Decides whether the ✕ cancels or closes.
    #[must_use]
    pub const fn is_running(self) -> bool {
        !matches!(self, Self::Result | Self::Failed)
    }
}

/// What the measurements came to.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CalibrationResultView {
    /// The floor during the silence phase, dBFS.
    pub floor_db: f32,
    /// The RMS of the speech phase, dBFS.
    pub speech_rms_db: f32,
    /// The peak of the speech phase, dBFS.
    pub speech_peak_db: f32,
    /// How much of the loud phase clipped, in percent.
    pub clipped_percent: f32,
    /// The shipped voice preset the recommendation starts from — shown as written, like every
    /// preset name.
    pub preset: String,
    /// The recommended settings, one per line and already translated (`High-pass 80 Hz`, …).
    /// At most [`wizard::MAX_LINES`] are shown.
    pub lines: Vec<String>,
}

/// Everything the wizard draws.
#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationView {
    pub phase: CalibrationPhase,
    /// Seconds left in a timed phase; shown rounded up, so a phase never reads `0` while it runs.
    pub seconds_left: f32,
    /// How much of the current phase has gone, `0.0..=1.0` — the countdown bar's fill. The
    /// analysis may use it too, for however long that takes.
    pub phase_fraction: f32,
    /// The microphone's level right now, dBFS: the live bar under the countdown.
    pub level_db: f32,
    /// Set once there is something to show; the Result phase draws dashes without it.
    pub result: Option<CalibrationResultView>,
    /// The microphone's description (`node.description`), under the title.
    pub device: String,
    /// Why the run failed, already translated. Shown in the Failed phase only.
    pub failure: String,
    /// Whether the host can take the measurements: the input lane has a microphone and something
    /// times the phases and reads its meters — the host wakes the lane itself if it is idle.
    /// Start and Retry stay disabled without it, so the wizard never counts down over a microphone
    /// nobody is listening to. [`Self::intro`] leaves it `false`; the host says when it can.
    pub can_measure: bool,
}

impl Default for CalibrationView {
    fn default() -> Self {
        Self::intro(String::new())
    }
}

impl CalibrationView {
    /// The first page, for `device`, with Start disabled until the host sets
    /// [`Self::can_measure`].
    #[must_use]
    pub fn intro(device: impl Into<String>) -> Self {
        Self {
            phase: CalibrationPhase::Intro,
            seconds_left: 0.0,
            phase_fraction: 0.0,
            level_db: wizard::LEVEL_RANGE_DB.0,
            result: None,
            device: device.into(),
            failure: String::new(),
            can_measure: false,
        }
    }

    /// The outer size the wizard wants in its current phase — the result is taller than the
    /// rest, so a host has to resize between them, as it does for the Import dialog's summary.
    #[must_use]
    pub fn window_size(&self) -> Vec2 {
        if self.phase == CalibrationPhase::Result {
            wizard::RESULT_WINDOW_SIZE
        } else {
            wizard::WINDOW_SIZE
        }
    }

    /// Whether Start — and Retry, which starts again — can do anything: there has to be a
    /// microphone to listen to and a host that can listen to it.
    #[must_use]
    pub fn can_start(&self) -> bool {
        self.can_measure && !self.device.trim().is_empty()
    }

    /// Whether Apply can do anything: there has to be a result to apply.
    #[must_use]
    pub const fn can_apply(&self) -> bool {
        self.result.is_some()
    }

    /// The countdown as shown: whole seconds, rounded up, never negative.
    #[must_use]
    pub fn seconds_text(&self) -> String {
        let seconds = if self.seconds_left.is_finite() {
            self.seconds_left.max(0.0).ceil()
        } else {
            0.0
        };
        format!("{seconds:.0}")
    }

    /// How far to fill the live level bar, `0.0..=1.0`, over [`wizard::LEVEL_RANGE_DB`].
    #[must_use]
    pub fn level_fill(&self) -> f32 {
        let (low, high) = wizard::LEVEL_RANGE_DB;
        if self.level_db.is_nan() {
            return 0.0;
        }
        ((self.level_db - low) / (high - low)).clamp(0.0, 1.0)
    }
}

/// What the user pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CalibrationAction {
    /// Begin the silence phase.
    Start,
    /// Stop listening and leave without applying anything — Cancel, or the ✕ / Escape while the
    /// wizard is running.
    Cancel,
    /// Write the recommended preset and select it.
    Apply,
    /// Measure again from the silence phase.
    Retry,
    /// Leave a finished or failed wizard without applying — OK on a failure, or the ✕ / Escape.
    Close,
}

// =============================================================================================
// Geometry
// =============================================================================================

/// The wizard's geometry. The original has no such window, so the numbers are borrowed from the
/// Import dialog it is hosted like: 400 points of content, twenty of margin, 80 × 30 buttons in
/// the bottom-right corner, ten from the edges.
pub mod wizard {
    use egui::{Vec2, vec2};

    /// Every phase but the result.
    pub const CONTENT_SIZE: Vec2 = vec2(400.0, 170.0);
    /// Outer size, from the shared formula.
    pub const WINDOW_SIZE: Vec2 = vec2(410.0, 257.0);
    /// The result: the five-row table and up to six lines of recommendation.
    pub const RESULT_CONTENT_SIZE: Vec2 = vec2(400.0, 320.0);
    pub const RESULT_WINDOW_SIZE: Vec2 = vec2(410.0, 407.0);
    /// Left and right margin, the Import dialog's.
    pub const MARGIN: f32 = 20.0;
    /// The device line under the title bar.
    pub const TEXT_HEIGHT: f32 = 20.0;
    /// The instruction, in the normal font.
    pub const INSTRUCTION_HEIGHT: f32 = 24.0;
    /// The countdown's digits, right-aligned on the instruction's row.
    pub const SECONDS_WIDTH: f32 = 40.0;
    /// Four lines of the small font: the introduction, which is three in English and needs the
    /// fourth in the longer translations.
    pub const INTRO_HEIGHT: f32 = 80.0;
    /// Two lines of the small font: why a run failed.
    pub const FAILURE_HEIGHT: f32 = 40.0;
    /// The countdown bar: the export progress bar's colours, twice its height so it reads as a
    /// countdown rather than a rule.
    pub const COUNTDOWN_HEIGHT: f32 = 4.0;
    /// The live level row: a bar in the readout strip's style and the level in dB beside it.
    pub const LEVEL_ROW_HEIGHT: f32 = 20.0;
    pub const LEVEL_BAR_HEIGHT: f32 = 4.0;
    pub const LEVEL_TEXT_WIDTH: f32 = 60.0;
    pub const LEVEL_BAR_GAP: f32 = 6.0;
    /// The bar spans this range, dBFS: −60 is a quiet room through a decent microphone, 0 is the
    /// converter's full scale.
    pub const LEVEL_RANGE_DB: (f32, f32) = (-60.0, 0.0);
    /// The Import dialog's button (`FxPresetImportDialog.h:53-54`): the narrowest a button is.
    pub const BUTTON_SIZE: Vec2 = vec2(80.0, 30.0);
    /// A translation longer than the button grows it, by this much either side of the label…
    pub const BUTTON_PADDING: f32 = 12.0;
    /// …up to this, where two buttons and their gap still fit the working width.
    pub const MAX_BUTTON_WIDTH: f32 = 170.0;
    pub const BUTTON_GAP: f32 = 10.0;
    /// The result table: five rows of label and value.
    pub const TABLE_ROWS: usize = 5;
    pub const TABLE_ROW_HEIGHT: f32 = 22.0;
    /// The recommendation, one small-font line each, under the table.
    pub const LINE_HEIGHT: f32 = 18.0;
    pub const MAX_LINES: usize = 6;
}

/// The introduction.
pub const INTRO_TEXT: &str = "Stay quiet, then speak normally, then speak loudly. FxSound measures \
your microphone and suggests voice settings for it.";
/// The three measurements' instructions. Written out rather than built from a number, so that
/// each translation can put its own plural on its own count.
pub const STAY_QUIET: &str = "Stay quiet for 3 seconds";
pub const SPEAK_NORMALLY: &str = "Speak normally for 5 seconds";
pub const SPEAK_LOUDLY: &str = "Speak loudly for 2 seconds";
/// While the recommendation is worked out.
pub const ANALYSING: &str = "Analysing…";
/// The Failed phase's heading.
pub const FAILED_TITLE: &str = "Calibration failed";

/// The device line: the first row of every phase.
#[must_use]
pub fn device_rect(content: Rect) -> Rect {
    Rect::from_min_size(
        pos2(content.left() + wizard::MARGIN, content.top() + 10.0),
        vec2(content.width() - wizard::MARGIN * 2.0, wizard::TEXT_HEIGHT),
    )
}

/// The introduction's text, ten points under the device line.
#[must_use]
pub fn intro_rect(content: Rect) -> Rect {
    let device = device_rect(content);
    Rect::from_min_size(
        pos2(device.left(), device.bottom() + 10.0),
        vec2(device.width(), wizard::INTRO_HEIGHT),
    )
}

/// The instruction (or the failure's heading), where the introduction's text starts.
#[must_use]
pub fn instruction_rect(content: Rect) -> Rect {
    let device = device_rect(content);
    Rect::from_min_size(
        pos2(device.left(), device.bottom() + 10.0),
        vec2(
            device.width() - wizard::SECONDS_WIDTH,
            wizard::INSTRUCTION_HEIGHT,
        ),
    )
}

/// The countdown's digits, at the right end of the instruction's row.
#[must_use]
pub fn seconds_rect(content: Rect) -> Rect {
    let instruction = instruction_rect(content);
    Rect::from_min_size(
        pos2(instruction.right(), instruction.top()),
        vec2(wizard::SECONDS_WIDTH, instruction.height()),
    )
}

/// The countdown bar, ten points under the instruction and the whole working width.
#[must_use]
pub fn countdown_rect(content: Rect) -> Rect {
    let device = device_rect(content);
    Rect::from_min_size(
        pos2(device.left(), instruction_rect(content).bottom() + 10.0),
        vec2(device.width(), wizard::COUNTDOWN_HEIGHT),
    )
}

/// The live level's row, twelve points under the countdown.
#[must_use]
pub fn level_row_rect(content: Rect) -> Rect {
    let countdown = countdown_rect(content);
    Rect::from_min_size(
        pos2(countdown.left(), countdown.bottom() + 12.0),
        vec2(countdown.width(), wizard::LEVEL_ROW_HEIGHT),
    )
}

/// The level in dB, at the right end of its row.
#[must_use]
pub fn level_text_rect(content: Rect) -> Rect {
    let row = level_row_rect(content);
    Rect::from_min_size(
        pos2(row.right() - wizard::LEVEL_TEXT_WIDTH, row.top()),
        vec2(wizard::LEVEL_TEXT_WIDTH, row.height()),
    )
}

/// The level bar, from the row's left edge to a gap short of the number, vertically centred.
#[must_use]
pub fn level_bar_rect(content: Rect) -> Rect {
    let row = level_row_rect(content);
    let text = level_text_rect(content);
    Rect::from_min_max(
        pos2(row.left(), row.center().y - wizard::LEVEL_BAR_HEIGHT / 2.0),
        pos2(
            text.left() - wizard::LEVEL_BAR_GAP,
            row.center().y + wizard::LEVEL_BAR_HEIGHT / 2.0,
        ),
    )
}

/// Why the run failed, under its heading.
#[must_use]
pub fn failure_rect(content: Rect) -> Rect {
    let device = device_rect(content);
    Rect::from_min_size(
        pos2(device.left(), instruction_rect(content).bottom() + 10.0),
        vec2(device.width(), wizard::FAILURE_HEIGHT),
    )
}

/// How wide a button is for a label `label_width` points wide: Import's 80, unless the label
/// needs more.
#[must_use]
pub fn button_width(label_width: f32) -> f32 {
    (label_width + wizard::BUTTON_PADDING * 2.0)
        .clamp(wizard::BUTTON_SIZE.x, wizard::MAX_BUTTON_WIDTH)
}

/// The primary button, `width` wide: bottom-right, as Import's is
/// (`FxPresetImportDialog.cpp:250-252`).
#[must_use]
pub fn primary_button_rect(content: Rect, width: f32) -> Rect {
    Rect::from_min_size(
        pos2(
            content.right() - wizard::MARGIN - width,
            content.bottom() - 10.0 - wizard::BUTTON_SIZE.y,
        ),
        vec2(width, wizard::BUTTON_SIZE.y),
    )
}

/// The secondary button, `width` wide, a gap to the left of a primary `primary_width` wide.
#[must_use]
pub fn secondary_button_rect(content: Rect, primary_width: f32, width: f32) -> Rect {
    let primary = primary_button_rect(content, primary_width);
    Rect::from_min_size(
        pos2(primary.left() - wizard::BUTTON_GAP - width, primary.top()),
        vec2(width, wizard::BUTTON_SIZE.y),
    )
}

/// The one or two buttons a phase shows, secondary first, as their labels and where they go.
fn buttons<'a>(ui: &Ui, content: Rect, labels: &[&'a str]) -> Vec<(&'a str, Rect)> {
    let widths: Vec<f32> = labels
        .iter()
        .map(|label| {
            let font = super::text_button_font(wizard::BUTTON_SIZE.y);
            button_width(
                ui.painter()
                    .layout_no_wrap((*label).to_owned(), font, egui::Color32::PLACEHOLDER)
                    .size()
                    .x,
            )
        })
        .collect();
    match (labels, widths.as_slice()) {
        ([primary], [width]) => vec![(*primary, primary_button_rect(content, *width))],
        ([secondary, primary], [secondary_width, primary_width]) => vec![
            (
                *secondary,
                secondary_button_rect(content, *primary_width, *secondary_width),
            ),
            (*primary, primary_button_rect(content, *primary_width)),
        ],
        _ => Vec::new(),
    }
}

/// One row of the result table: label on the left, value on the right.
#[must_use]
pub fn table_row_rect(content: Rect, index: usize) -> Rect {
    let device = device_rect(content);
    Rect::from_min_size(
        pos2(
            device.left(),
            device.bottom() + 10.0 + index as f32 * wizard::TABLE_ROW_HEIGHT,
        ),
        vec2(device.width(), wizard::TABLE_ROW_HEIGHT),
    )
}

/// One line of the recommendation, ten points under the table.
#[must_use]
pub fn line_rect(content: Rect, index: usize) -> Rect {
    let table = table_row_rect(content, wizard::TABLE_ROWS - 1);
    Rect::from_min_size(
        pos2(
            table.left(),
            table.bottom() + 10.0 + index as f32 * wizard::LINE_HEIGHT,
        ),
        vec2(table.width(), wizard::LINE_HEIGHT),
    )
}

/// The result table's five rows, as label and value. Dashes stand in for a result that has not
/// arrived.
#[must_use]
pub fn table_rows(result: Option<&CalibrationResultView>) -> [(String, String); 5] {
    const DASH: &str = "—";
    let value = |f: &dyn Fn(&CalibrationResultView) -> String| result.map_or(DASH.to_owned(), f);
    [
        (tr("Floor"), value(&|r| whole_db(r.floor_db))),
        (tr("Speech"), value(&|r| whole_db(r.speech_rms_db))),
        (tr("Peak"), value(&|r| whole_db(r.speech_peak_db))),
        (
            tr("Clipping"),
            value(&|r| {
                let percent = if r.clipped_percent.is_finite() {
                    r.clipped_percent.clamp(0.0, 100.0)
                } else {
                    0.0
                };
                format!("{percent:.1} %")
            }),
        ),
        (tr("Preset"), value(&|r| r.preset.clone())),
    ]
}

// =============================================================================================
// The dialog
// =============================================================================================

/// The calibration wizard.
pub struct CalibrationDialog<'a> {
    view: &'a CalibrationView,
}

impl<'a> CalibrationDialog<'a> {
    #[must_use]
    pub fn new(view: &'a CalibrationView) -> Self {
        Self { view }
    }

    /// Draw the wizard into `outer`, which should be [`CalibrationView::window_size`].
    pub fn show(
        self,
        ui: &mut Ui,
        outer: Rect,
        palette: Palette,
        assets: &mut AssetCache,
        id_salt: impl std::hash::Hash + std::fmt::Debug,
    ) -> DialogResponse<CalibrationAction> {
        let view = self.view;
        let id = Id::new("fx_calibration_dialog").with(id_salt);
        let mut response = DialogResponse::default();

        let chrome = DialogChrome::titled(&tr("Calibrate microphone")).show(
            ui,
            outer,
            palette,
            assets,
            id.with("chrome"),
        );
        let dismiss = if view.phase.is_running() {
            CalibrationAction::Cancel
        } else {
            CalibrationAction::Close
        };
        response.push_if(chrome.close_clicked, dismiss);
        response.push_if(ui.input(|i| i.key_pressed(Key::Escape)), dismiss);

        let content = chrome.content;
        draw_truncated(
            ui.painter(),
            &view.device,
            small_font(),
            palette.color(FxColor::DefaultText),
            device_rect(content),
            Align2::LEFT_CENTER,
        );

        match view.phase {
            CalibrationPhase::Intro => intro(ui, content, view, palette, id, &mut response),
            CalibrationPhase::Silence | CalibrationPhase::Speech | CalibrationPhase::Loud => {
                measuring(ui, content, view, palette, id, &mut response);
            }
            CalibrationPhase::Analysing => analysing(ui, content, view, palette, id, &mut response),
            CalibrationPhase::Result => result(ui, content, view, palette, id, &mut response),
            CalibrationPhase::Failed => failed(ui, content, view, palette, id, &mut response),
        }
        response
    }
}

fn intro(
    ui: &mut Ui,
    content: Rect,
    view: &CalibrationView,
    palette: Palette,
    id: Id,
    response: &mut DialogResponse<CalibrationAction>,
) {
    draw_wrapped(
        ui.painter(),
        &tr(INTRO_TEXT),
        small_font(),
        palette.color(FxColor::DefaultText),
        intro_rect(content),
    );
    let (cancel, start) = (tr("Cancel"), tr("Start"));
    let placed = buttons(ui, content, &[cancel.as_str(), start.as_str()]);
    button_row(
        ui,
        &placed,
        &[
            (CalibrationAction::Cancel, true),
            (CalibrationAction::Start, view.can_start()),
        ],
        palette,
        id,
        response,
    );
}

fn measuring(
    ui: &mut Ui,
    content: Rect,
    view: &CalibrationView,
    palette: Palette,
    id: Id,
    response: &mut DialogResponse<CalibrationAction>,
) {
    let instruction = match view.phase {
        CalibrationPhase::Silence => tr(STAY_QUIET),
        CalibrationPhase::Speech => tr(SPEAK_NORMALLY),
        _ => tr(SPEAK_LOUDLY),
    };
    heading(ui, content, &instruction, palette);
    draw_truncated(
        ui.painter(),
        &view.seconds_text(),
        normal_font(),
        palette.color(FxColor::HighlightedText),
        seconds_rect(content),
        Align2::RIGHT_CENTER,
    );
    paint_countdown(ui, countdown_rect(content), palette, view.phase_fraction);

    paint_level_bar(ui, level_bar_rect(content), palette, view.level_fill());
    draw_truncated(
        ui.painter(),
        &whole_db(view.level_db),
        small_font(),
        palette.color(FxColor::DefaultText),
        level_text_rect(content),
        Align2::RIGHT_CENTER,
    );

    cancel_only(ui, content, palette, id, response);
}

fn analysing(
    ui: &mut Ui,
    content: Rect,
    view: &CalibrationView,
    palette: Palette,
    id: Id,
    response: &mut DialogResponse<CalibrationAction>,
) {
    heading(ui, content, &tr(ANALYSING), palette);
    paint_countdown(ui, countdown_rect(content), palette, view.phase_fraction);
    cancel_only(ui, content, palette, id, response);
}

fn result(
    ui: &mut Ui,
    content: Rect,
    view: &CalibrationView,
    palette: Palette,
    id: Id,
    response: &mut DialogResponse<CalibrationAction>,
) {
    for (index, (label, value)) in table_rows(view.result.as_ref()).into_iter().enumerate() {
        let row = table_row_rect(content, index);
        // The value first, so the label knows how much room it has left.
        let value_rect = draw_truncated(
            ui.painter(),
            &value,
            normal_font(),
            palette.color(FxColor::HighlightedText),
            row,
            Align2::RIGHT_CENTER,
        );
        let label_rect = Rect::from_min_max(row.min, pos2(value_rect.left() - 10.0, row.bottom()));
        draw_truncated(
            ui.painter(),
            &label,
            small_font(),
            palette.color(FxColor::DefaultText),
            label_rect,
            Align2::LEFT_CENTER,
        );
    }

    let lines = view.result.as_ref().map_or(&[][..], |r| r.lines.as_slice());
    for (index, line) in lines.iter().take(wizard::MAX_LINES).enumerate() {
        draw_truncated(
            ui.painter(),
            line,
            small_font(),
            palette.color(FxColor::DefaultText),
            line_rect(content, index),
            Align2::LEFT_CENTER,
        );
    }

    let (retry, apply) = (tr("Retry"), tr("Apply"));
    let placed = buttons(ui, content, &[retry.as_str(), apply.as_str()]);
    button_row(
        ui,
        &placed,
        &[
            (CalibrationAction::Retry, view.can_start()),
            (CalibrationAction::Apply, view.can_apply()),
        ],
        palette,
        id,
        response,
    );
}

fn failed(
    ui: &mut Ui,
    content: Rect,
    view: &CalibrationView,
    palette: Palette,
    id: Id,
    response: &mut DialogResponse<CalibrationAction>,
) {
    heading(ui, content, &tr(FAILED_TITLE), palette);
    draw_wrapped(
        ui.painter(),
        &view.failure,
        small_font(),
        palette.color(FxColor::DefaultText),
        failure_rect(content),
    );
    let (ok, retry) = (tr("OK"), tr("Retry"));
    let placed = buttons(ui, content, &[ok.as_str(), retry.as_str()]);
    button_row(
        ui,
        &placed,
        &[
            (CalibrationAction::Close, true),
            (CalibrationAction::Retry, view.can_start()),
        ],
        palette,
        id,
        response,
    );
}

/// The instruction, or the heading of a phase that has none.
fn heading(ui: &Ui, content: Rect, text: &str, palette: Palette) {
    draw_truncated(
        ui.painter(),
        text,
        normal_font(),
        palette.color(FxColor::HighlightedText),
        instruction_rect(content),
        Align2::LEFT_CENTER,
    );
}

/// Cancel alone in the corner, while the wizard listens or thinks: the only thing left to do.
fn cancel_only(
    ui: &mut Ui,
    content: Rect,
    palette: Palette,
    id: Id,
    response: &mut DialogResponse<CalibrationAction>,
) {
    let cancel = tr("Cancel");
    let placed = buttons(ui, content, &[cancel.as_str()]);
    button_row(
        ui,
        &placed,
        &[(CalibrationAction::Cancel, true)],
        palette,
        id,
        response,
    );
}

/// Draw placed buttons, each with the action it emits and whether it is enabled.
fn button_row(
    ui: &mut Ui,
    placed: &[(&str, Rect)],
    actions: &[(CalibrationAction, bool)],
    palette: Palette,
    id: Id,
    response: &mut DialogResponse<CalibrationAction>,
) {
    for ((label, rect), (action, enabled)) in placed.iter().zip(actions) {
        if TextButton::new(label)
            .enabled(*enabled)
            .show(ui, *rect, palette, id.with(("button", *action)))
            .clicked()
        {
            response.push(*action);
        }
    }
}

/// The countdown: an unlit track, and the part of the phase that has gone in the export progress
/// bar's ramp, `ImageButton` at the left to `VerticalSliderLow` at the leading edge
/// (`FxPresetExportDialog.cpp:62-71`).
fn paint_countdown(ui: &Ui, rect: Rect, palette: Palette, fraction: f32) {
    let corner = CornerRadius::same((rect.height() / 2.0) as u8);
    ui.painter().rect_filled(
        rect,
        corner,
        palette.color_alpha(FxColor::DefaultText, 0.15),
    );
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if fraction <= 0.0 {
        return;
    }
    let edge = rect.left() + rect.width() * fraction;
    let start = palette.color(FxColor::ImageButton);
    let end = palette.color(FxColor::VerticalSliderLow);
    let mut mesh = Mesh::default();
    mesh.colored_vertex(pos2(rect.left(), rect.top()), start);
    mesh.colored_vertex(pos2(rect.left(), rect.bottom()), start);
    mesh.colored_vertex(pos2(edge, rect.top()), end);
    mesh.colored_vertex(pos2(edge, rect.bottom()), end);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(2, 1, 3);
    ui.painter().add(Shape::mesh(mesh));
}

/// The live level, in the readout strip's bar: an unlit track in `DefaultText` at 15 %, the lit
/// part in `HighlightedText`.
fn paint_level_bar(ui: &Ui, rect: Rect, palette: Palette, fill: f32) {
    ui.painter()
        .rect_filled(rect, 1.0, palette.color_alpha(FxColor::DefaultText, 0.15));
    if fill > 0.0 {
        let mut lit = rect;
        lit.set_width(rect.width() * fill.clamp(0.0, 1.0));
        ui.painter()
            .rect_filled(lit, 1.0, palette.color(FxColor::HighlightedText));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{frame, test_context};
    use super::*;
    use egui::{Event, PointerButton, RawInput};
    use fxsound_core::ThemeMode;

    fn content(view: &CalibrationView) -> Rect {
        Rect::from_min_size(
            pos2(5.0, 62.0),
            super::super::content_size(view.window_size()),
        )
    }

    fn view(phase: CalibrationPhase) -> CalibrationView {
        CalibrationView {
            phase,
            seconds_left: 2.4,
            phase_fraction: 0.4,
            level_db: -24.0,
            result: (phase == CalibrationPhase::Result).then(sample_result),
            failure: if phase == CalibrationPhase::Failed {
                "The microphone sent nothing but silence.".to_owned()
            } else {
                String::new()
            },
            can_measure: true,
            ..CalibrationView::intro("fifine Microphone Analogue Stereo")
        }
    }

    fn sample_result() -> CalibrationResultView {
        CalibrationResultView {
            floor_db: -48.3,
            speech_rms_db: -19.4,
            speech_peak_db: -6.2,
            clipped_percent: 0.04,
            preset: "Headset".to_owned(),
            lines: vec![
                "High-pass 80 Hz".to_owned(),
                "Gate −40 dB".to_owned(),
                "Compressor −25 dB".to_owned(),
                "Makeup +3 dB".to_owned(),
                "Ceiling −3 dB".to_owned(),
                "Noise suppression Mild".to_owned(),
            ],
        }
    }

    /// Every rectangle the phase draws into, by name.
    fn drawn(view: &CalibrationView) -> Vec<(&'static str, Rect)> {
        let c = content(view);
        let mut rects = vec![("device", device_rect(c))];
        match view.phase {
            CalibrationPhase::Intro => rects.extend([
                ("intro", intro_rect(c)),
                ("cancel", secondary_button_rect(c, 80.0, 80.0)),
                ("start", primary_button_rect(c, 80.0)),
            ]),
            CalibrationPhase::Silence | CalibrationPhase::Speech | CalibrationPhase::Loud => rects
                .extend([
                    ("instruction", instruction_rect(c)),
                    ("seconds", seconds_rect(c)),
                    ("countdown", countdown_rect(c)),
                    ("level bar", level_bar_rect(c)),
                    ("level", level_text_rect(c)),
                    ("cancel", primary_button_rect(c, 80.0)),
                ]),
            CalibrationPhase::Analysing => rects.extend([
                ("instruction", instruction_rect(c)),
                ("countdown", countdown_rect(c)),
                ("cancel", primary_button_rect(c, 80.0)),
            ]),
            CalibrationPhase::Result => {
                rects.extend((0..wizard::TABLE_ROWS).map(|i| ("table row", table_row_rect(c, i))));
                rects.extend((0..wizard::MAX_LINES).map(|i| ("line", line_rect(c, i))));
                rects.extend([
                    ("retry", secondary_button_rect(c, 80.0, 80.0)),
                    ("apply", primary_button_rect(c, 80.0)),
                ]);
            }
            CalibrationPhase::Failed => rects.extend([
                ("heading", instruction_rect(c)),
                ("failure", failure_rect(c)),
                ("ok", secondary_button_rect(c, 80.0, 80.0)),
                ("retry", primary_button_rect(c, 80.0)),
            ]),
        }
        rects
    }

    fn raw_input(view: &CalibrationView, events: Vec<Event>) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), view.window_size())),
            events,
            ..Default::default()
        }
    }

    /// One frame of the wizard, filling its whole window.
    fn run(
        ctx: &egui::Context,
        assets: &mut AssetCache,
        view: &CalibrationView,
        events: Vec<Event>,
    ) -> Vec<CalibrationAction> {
        let mut actions = Vec::new();
        ctx.run_ui(raw_input(view, events), |ui| {
            let outer = Rect::from_min_size(pos2(0.0, 0.0), view.window_size());
            actions = CalibrationDialog::new(view)
                .show(ui, outer, Palette::new(ThemeMode::Dark), assets, "test")
                .actions;
        })
        .drop_without_applying_deltas();
        actions
    }

    /// Move to `at`, press and release, and collect whatever the three frames emitted.
    fn click(view: &CalibrationView, at: egui::Pos2) -> Vec<CalibrationAction> {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let button = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        let mut actions = Vec::new();
        for events in [
            vec![Event::PointerMoved(at)],
            vec![Event::PointerMoved(at), button(true)],
            vec![button(false)],
        ] {
            actions.extend(run(&ctx, &mut assets, view, events));
        }
        actions
    }

    fn escape(view: &CalibrationView) -> Vec<CalibrationAction> {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        run(
            &ctx,
            &mut assets,
            view,
            vec![Event::Key {
                key: Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        )
    }

    fn close_button(view: &CalibrationView) -> egui::Pos2 {
        let outer = Rect::from_min_size(pos2(0.0, 0.0), view.window_size());
        super::super::close_button_rect(super::super::title_bar_rect(outer)).center()
    }

    #[test]
    fn every_window_size_follows_the_shared_formula() {
        assert!(
            (super::super::outer_size(wizard::CONTENT_SIZE) - wizard::WINDOW_SIZE).length() < 1e-4
        );
        assert!(
            (super::super::outer_size(wizard::RESULT_CONTENT_SIZE) - wizard::RESULT_WINDOW_SIZE)
                .length()
                < 1e-4
        );
    }

    #[test]
    fn only_the_result_asks_for_the_taller_window() {
        for phase in CalibrationPhase::ALL {
            let expected = if phase == CalibrationPhase::Result {
                wizard::RESULT_WINDOW_SIZE
            } else {
                wizard::WINDOW_SIZE
            };
            assert_eq!(view(phase).window_size(), expected, "{phase:?}");
        }
    }

    #[test]
    fn the_wizard_fits_inside_the_settings_window_it_opens_over() {
        // Drawn over Settings, so neither shape may grow the main window past what Settings
        // already made it — and the Pro window is roomier still in width.
        let settings = super::super::settings::WINDOW_SIZE;
        let pro = crate::layout::pro::WINDOW_SIZE;
        for size in [wizard::WINDOW_SIZE, wizard::RESULT_WINDOW_SIZE] {
            assert!(
                size.x <= settings.x && size.y <= settings.y,
                "{size:?} > {settings:?}"
            );
            assert!(size.x <= pro.x && size.y <= pro.y, "{size:?} > {pro:?}");
        }
    }

    #[test]
    fn every_phase_keeps_everything_inside_its_content_and_nothing_overlaps() {
        for phase in CalibrationPhase::ALL {
            let view = view(phase);
            let c = content(&view);
            let rects = drawn(&view);
            for (name, rect) in &rects {
                assert!(
                    c.contains_rect(*rect),
                    "{phase:?}: {name} {rect:?} leaves the content {c:?}"
                );
                // Twenty points of margin either side, as the Import dialog keeps.
                assert!(
                    rect.left() >= c.left() + wizard::MARGIN - 1e-4,
                    "{phase:?}: {name}"
                );
                assert!(
                    rect.right() <= c.right() - wizard::MARGIN + 1e-4,
                    "{phase:?}: {name}"
                );
            }
            for (i, (a_name, a)) in rects.iter().enumerate() {
                for (b_name, b) in &rects[i + 1..] {
                    assert!(
                        !a.intersects(*b) || a.intersect(*b).area() < 1e-3,
                        "{phase:?}: {a_name} {a:?} overlaps {b_name} {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_buttons_sit_in_the_bottom_right_corner_primary_rightmost() {
        let c = content(&view(CalibrationPhase::Intro));
        let primary = primary_button_rect(c, 80.0).translate(-c.min.to_vec2());
        let secondary = secondary_button_rect(c, 80.0, 80.0).translate(-c.min.to_vec2());
        // (300, 130, 80, 30) and (210, 130, 80, 30): Import's button and its neighbour.
        assert!(
            (primary.min - pos2(300.0, 130.0)).length() < 1e-4,
            "{primary:?}"
        );
        assert!(
            (secondary.min - pos2(210.0, 130.0)).length() < 1e-4,
            "{secondary:?}"
        );
        assert!((primary.size() - vec2(80.0, 30.0)).length() < 1e-4);
        assert!((primary.left() - secondary.right() - 10.0).abs() < 1e-4);
    }

    #[test]
    fn the_measuring_rows_stack_in_order_under_the_device_line() {
        let c = content(&view(CalibrationPhase::Speech));
        let local = |r: Rect| r.translate(-c.min.to_vec2());
        assert!((local(device_rect(c)).min - pos2(20.0, 10.0)).length() < 1e-4);
        assert!((local(instruction_rect(c)).min - pos2(20.0, 40.0)).length() < 1e-4);
        assert!((local(countdown_rect(c)).min - pos2(20.0, 74.0)).length() < 1e-4);
        assert!((countdown_rect(c).width() - 360.0).abs() < 1e-4);
        assert!((local(level_row_rect(c)).min - pos2(20.0, 90.0)).length() < 1e-4);
        // The digits share the instruction's row and end at the margin.
        assert!((seconds_rect(c).top() - instruction_rect(c).top()).abs() < 1e-4);
        assert!((seconds_rect(c).right() - (c.right() - 20.0)).abs() < 1e-4);
        // The level bar stops short of its number, and is the strip's four points tall.
        assert!(level_bar_rect(c).right() < level_text_rect(c).left());
        assert!((level_bar_rect(c).height() - 4.0).abs() < 1e-4);
        assert!(level_row_rect(c).bottom() < primary_button_rect(c, 80.0).top());
    }

    #[test]
    fn the_result_table_has_five_rows_and_room_for_six_lines_above_the_buttons() {
        let c = content(&view(CalibrationPhase::Result));
        for pair in (0..wizard::TABLE_ROWS).collect::<Vec<_>>().windows(2) {
            let (a, b) = (table_row_rect(c, pair[0]), table_row_rect(c, pair[1]));
            assert!((b.top() - a.bottom()).abs() < 1e-4, "the table rows touch");
        }
        let last_line = line_rect(c, wizard::MAX_LINES - 1);
        assert!(line_rect(c, 0).top() >= table_row_rect(c, wizard::TABLE_ROWS - 1).bottom());
        assert!(last_line.bottom() <= primary_button_rect(c, 80.0).top());
    }

    #[test]
    fn a_phase_is_timed_only_while_it_listens() {
        assert_eq!(CalibrationPhase::Silence.seconds(), 3.0);
        assert_eq!(CalibrationPhase::Speech.seconds(), 5.0);
        assert_eq!(CalibrationPhase::Loud.seconds(), 2.0);
        for phase in CalibrationPhase::ALL {
            assert_eq!(phase.is_measuring(), phase.seconds() > 0.0, "{phase:?}");
        }
        // Everything before a result or a failure is still running, and so can be cancelled.
        let running: Vec<_> = CalibrationPhase::ALL
            .into_iter()
            .filter(|p| p.is_running())
            .collect();
        assert_eq!(
            running,
            [
                CalibrationPhase::Intro,
                CalibrationPhase::Silence,
                CalibrationPhase::Speech,
                CalibrationPhase::Loud,
                CalibrationPhase::Analysing,
            ]
        );
    }

    #[test]
    fn the_countdown_rounds_up_and_never_reads_below_zero() {
        let mut v = view(CalibrationPhase::Silence);
        for (left, shown) in [(2.4, "3"), (3.0, "3"), (0.01, "1"), (0.0, "0"), (-0.5, "0")] {
            v.seconds_left = left;
            assert_eq!(v.seconds_text(), shown, "{left}");
        }
        v.seconds_left = f32::NAN;
        assert_eq!(v.seconds_text(), "0");
    }

    #[test]
    fn the_level_bar_spans_sixty_decibels_below_full_scale() {
        let mut v = view(CalibrationPhase::Speech);
        for (db, fill) in [
            (-60.0, 0.0),
            (-30.0, 0.5),
            (0.0, 1.0),
            (-90.0, 0.0),
            (6.0, 1.0),
        ] {
            v.level_db = db;
            assert!(
                (v.level_fill() - fill).abs() < 1e-6,
                "{db} dB filled {}",
                v.level_fill()
            );
        }
        v.level_db = f32::NAN;
        assert_eq!(v.level_fill(), 0.0);
        v.level_db = f32::NEG_INFINITY;
        assert_eq!(v.level_fill(), 0.0);
    }

    #[test]
    fn the_table_reads_the_result_in_whole_decibels_and_dashes_without_one() {
        let rows = table_rows(Some(&sample_result()));
        let values: Vec<&str> = rows.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, ["−48 dB", "−19 dB", "−6 dB", "0.0 %", "Headset"]);
        let labels: Vec<&str> = rows.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["Floor", "Speech", "Peak", "Clipping", "Preset"]);

        let empty = table_rows(None);
        assert!(empty.iter().all(|(_, v)| v == "—"), "{empty:?}");

        let clipped = CalibrationResultView {
            clipped_percent: 250.0,
            ..sample_result()
        };
        assert_eq!(table_rows(Some(&clipped))[3].1, "100.0 %");
    }

    #[test]
    fn start_needs_a_microphone_and_a_host_that_can_measure_and_apply_needs_a_result() {
        assert!(view(CalibrationPhase::Intro).can_start());
        let blank = CalibrationView {
            can_measure: true,
            ..CalibrationView::intro("  ")
        };
        assert!(!blank.can_start(), "no microphone");
        let unheard = CalibrationView {
            can_measure: false,
            ..view(CalibrationPhase::Intro)
        };
        assert!(!unheard.can_start(), "nobody listening");
        assert!(view(CalibrationPhase::Result).can_apply());
        let empty = CalibrationView {
            phase: CalibrationPhase::Result,
            ..CalibrationView::default()
        };
        assert!(!empty.can_apply());
    }

    #[test]
    fn every_phase_draws_in_both_palettes_and_asks_for_nothing_on_its_own() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            for phase in CalibrationPhase::ALL {
                let view = view(phase);
                frame(&ctx, |ui| {
                    let outer = Rect::from_min_size(pos2(0.0, 0.0), view.window_size());
                    let response = CalibrationDialog::new(&view).show(
                        ui,
                        outer,
                        Palette::new(mode),
                        &mut assets,
                        "quiet",
                    );
                    assert!(
                        response.is_empty(),
                        "{phase:?} in {mode:?} emitted {:?}",
                        response.actions
                    );
                });
            }
        }
    }

    #[test]
    fn a_result_with_more_lines_than_fit_and_a_missing_result_still_draw() {
        let ctx = test_context();
        let mut assets = AssetCache::new();
        let mut long = view(CalibrationPhase::Result);
        if let Some(result) = &mut long.result {
            result.lines = (0..20).map(|i| format!("line {i}")).collect();
        }
        let missing = CalibrationView {
            phase: CalibrationPhase::Result,
            ..view(CalibrationPhase::Intro)
        };
        for view in [long, missing] {
            assert!(run(&ctx, &mut assets, &view, Vec::new()).is_empty());
        }
    }

    #[test]
    fn start_on_the_introduction_starts_and_cancel_cancels() {
        let v = view(CalibrationPhase::Intro);
        let c = content(&v);
        assert_eq!(
            click(&v, primary_button_rect(c, 80.0).center()),
            [CalibrationAction::Start]
        );
        assert_eq!(
            click(&v, secondary_button_rect(c, 80.0, 80.0).center()),
            [CalibrationAction::Cancel]
        );
    }

    #[test]
    fn start_without_a_microphone_does_nothing() {
        let v = CalibrationView {
            can_measure: true,
            ..CalibrationView::intro("")
        };
        let c = content(&v);
        assert!(click(&v, primary_button_rect(c, 80.0).center()).is_empty());
    }

    #[test]
    fn a_fresh_introduction_waits_for_the_host_to_say_it_can_measure() {
        let fresh = CalibrationView::intro("fifine Microphone Analogue Stereo");
        assert!(!fresh.can_measure);
        assert!(!fresh.can_start());
        assert!(!CalibrationView::default().can_measure);
    }

    #[test]
    fn start_does_nothing_until_the_host_can_measure_but_cancel_still_cancels() {
        let v = CalibrationView::intro("fifine Microphone Analogue Stereo");
        let c = content(&v);
        assert!(click(&v, primary_button_rect(c, 80.0).center()).is_empty());
        assert_eq!(
            click(&v, secondary_button_rect(c, 80.0, 80.0).center()),
            [CalibrationAction::Cancel]
        );
    }

    #[test]
    fn retry_does_nothing_once_the_host_can_no_longer_measure_but_apply_and_ok_still_work() {
        let result = CalibrationView {
            can_measure: false,
            ..view(CalibrationPhase::Result)
        };
        let c = content(&result);
        assert!(click(&result, secondary_button_rect(c, 80.0, 80.0).center()).is_empty());
        assert_eq!(
            click(&result, primary_button_rect(c, 80.0).center()),
            [CalibrationAction::Apply]
        );

        let failed = CalibrationView {
            can_measure: false,
            ..view(CalibrationPhase::Failed)
        };
        let c = content(&failed);
        assert!(click(&failed, primary_button_rect(c, 80.0).center()).is_empty());
        assert_eq!(
            click(&failed, secondary_button_rect(c, 80.0, 80.0).center()),
            [CalibrationAction::Close]
        );
    }

    #[test]
    fn cancel_stops_every_measurement_and_the_analysis() {
        for phase in [
            CalibrationPhase::Silence,
            CalibrationPhase::Speech,
            CalibrationPhase::Loud,
            CalibrationPhase::Analysing,
        ] {
            let v = view(phase);
            let c = content(&v);
            assert_eq!(
                click(&v, primary_button_rect(c, 80.0).center()),
                [CalibrationAction::Cancel],
                "{phase:?}"
            );
            // Nothing sits where the introduction's Cancel was.
            assert!(
                click(&v, secondary_button_rect(c, 80.0, 80.0).center()).is_empty(),
                "{phase:?}"
            );
        }
    }

    #[test]
    fn the_result_applies_and_retries() {
        let v = view(CalibrationPhase::Result);
        let c = content(&v);
        assert_eq!(
            click(&v, primary_button_rect(c, 80.0).center()),
            [CalibrationAction::Apply]
        );
        assert_eq!(
            click(&v, secondary_button_rect(c, 80.0, 80.0).center()),
            [CalibrationAction::Retry]
        );
    }

    #[test]
    fn apply_without_a_result_does_nothing() {
        let v = CalibrationView {
            phase: CalibrationPhase::Result,
            ..view(CalibrationPhase::Intro)
        };
        let c = content(&v);
        assert!(click(&v, primary_button_rect(c, 80.0).center()).is_empty());
    }

    #[test]
    fn a_failure_offers_ok_and_retry() {
        let v = view(CalibrationPhase::Failed);
        let c = content(&v);
        assert_eq!(
            click(&v, primary_button_rect(c, 80.0).center()),
            [CalibrationAction::Retry]
        );
        assert_eq!(
            click(&v, secondary_button_rect(c, 80.0, 80.0).center()),
            [CalibrationAction::Close]
        );
    }

    #[test]
    fn the_cross_and_escape_cancel_a_running_wizard_and_close_a_finished_one() {
        for phase in CalibrationPhase::ALL {
            let v = view(phase);
            let expected = if phase.is_running() {
                CalibrationAction::Cancel
            } else {
                CalibrationAction::Close
            };
            assert_eq!(click(&v, close_button(&v)), [expected], "✕ in {phase:?}");
            assert_eq!(escape(&v), [expected], "Escape in {phase:?}");
        }
    }

    #[test]
    fn every_instruction_fits_its_row_in_english() {
        // Measured with the real faces: an instruction that is elided in the source language
        // has been written too long.
        let ctx = test_context();
        let c = content(&view(CalibrationPhase::Silence));
        frame(&ctx, |ui| {
            for text in [
                STAY_QUIET,
                SPEAK_NORMALLY,
                SPEAK_LOUDLY,
                ANALYSING,
                FAILED_TITLE,
            ] {
                let width = ui
                    .painter()
                    .layout_no_wrap(text.to_owned(), normal_font(), egui::Color32::PLACEHOLDER)
                    .size()
                    .x;
                assert!(
                    width <= instruction_rect(c).width(),
                    "{text:?} is {width} points in a {} point row",
                    instruction_rect(c).width()
                );
            }
            let intro = ui
                .painter()
                .layout(
                    INTRO_TEXT.to_owned(),
                    small_font(),
                    egui::Color32::PLACEHOLDER,
                    intro_rect(c).width(),
                )
                .size()
                .y;
            assert!(
                intro <= intro_rect(c).height(),
                "the introduction wrapped to {intro} points in a {} point box",
                intro_rect(c).height()
            );
        });
    }

    #[test]
    fn every_language_fits_the_instructions_the_introduction_and_the_buttons() {
        let ctx = test_context();
        let c = content(&view(CalibrationPhase::Silence));
        let every = super::super::tests::every_translation;
        let mut problems = Vec::new();
        frame(&ctx, |ui| {
            let width = |text: &str, font| {
                ui.painter()
                    .layout_no_wrap(text.to_owned(), font, egui::Color32::PLACEHOLDER)
                    .size()
                    .x
            };
            for key in [
                STAY_QUIET,
                SPEAK_NORMALLY,
                SPEAK_LOUDLY,
                ANALYSING,
                FAILED_TITLE,
                "Calibrate microphone",
            ] {
                for (code, text) in every(key) {
                    let used = width(&text, normal_font());
                    let room = instruction_rect(c).width();
                    if used > room {
                        problems.push(format!("{code}: {text:?} is {used:.0} in {room:.0}"));
                    }
                }
            }
            for key in ["Start", "Cancel", "Apply", "Retry", "OK"] {
                for (code, text) in every(key) {
                    let used = width(&text, super::super::text_button_font(30.0));
                    let room = wizard::MAX_BUTTON_WIDTH - wizard::BUTTON_PADDING * 2.0;
                    if used > room {
                        problems.push(format!("{code}: button {text:?} is {used:.0} in {room:.0}"));
                    }
                }
            }
            for key in ["Floor", "Speech", "Peak", "Clipping", "Preset"] {
                for (code, text) in every(key) {
                    // Beside the widest value the table shows, a preset name.
                    let used =
                        width(&text, small_font()) + 10.0 + width("Laptop Mic", normal_font());
                    if used > table_row_rect(c, 0).width() {
                        problems.push(format!("{code}: {text:?} crowds its value"));
                    }
                }
            }
            for (code, text) in every(INTRO_TEXT) {
                let height = ui
                    .painter()
                    .layout(
                        text.clone(),
                        small_font(),
                        egui::Color32::PLACEHOLDER,
                        intro_rect(c).width(),
                    )
                    .size()
                    .y;
                if height > intro_rect(c).height() {
                    problems.push(format!("{code}: the introduction is {height:.0} tall"));
                }
            }
        });
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn a_button_is_imports_width_unless_its_label_needs_more() {
        assert_eq!(button_width(10.0), 80.0);
        assert_eq!(button_width(56.0), 80.0);
        assert_eq!(button_width(76.0), 100.0);
        assert_eq!(button_width(500.0), wizard::MAX_BUTTON_WIDTH);
        // Two of the widest still fit the working width with their gap.
        let c = content(&view(CalibrationPhase::Result));
        let widest = wizard::MAX_BUTTON_WIDTH;
        let secondary = secondary_button_rect(c, widest, widest);
        assert!(
            secondary.left() >= c.left() + wizard::MARGIN,
            "{secondary:?}"
        );
    }
}
