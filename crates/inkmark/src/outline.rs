//! The heading outline. It lists [`inkmark_parse::headings`] for the open
//! document and reports which one was clicked.

use eframe::egui::{self, FontId, RichText, ScrollArea, Sense, pos2, vec2};
use inkmark_buffer::Document;
use inkmark_parse::{Heading, ParseOutput};
use inkmark_view::theme;

/// Heading rows kept until the parse revision changes.
#[derive(Default)]
pub(crate) struct Outline {
    rows: Vec<Heading>,
    revision: Option<u64>,
}

impl Outline {
    /// Rebuilds the rows when `revision` is not the one they were built from.
    pub(crate) fn refresh(&mut self, revision: u64, doc: &Document, out: &ParseOutput) {
        if self.revision == Some(revision) {
            return;
        }
        self.rows = inkmark_parse::headings(doc, out);
        self.revision = Some(revision);
    }

    pub(crate) fn rows(&self) -> &[Heading] {
        &self.rows
    }
}

/// Width the outline takes when the window has room for it.
pub const PREFERRED_WIDTH: f32 = 200.0;
/// Narrowest the outline gets before the panes give up more room.
pub const MIN_WIDTH: f32 = 120.0;
/// Widest the outline keeps. A wider drag stops here.
pub const MAX_WIDTH: f32 = 480.0;
/// Gap between the outline and the panes, and between the sidebar and the panes.
pub const GAP: f32 = 4.0;

const PANES_RESERVE: f32 = 240.0;
const ROW_H: f32 = 22.0;
const INDENT: f32 = 12.0;

/// Sidebar width (0 when the sidebar is hidden) and outline width for a
/// window `total` pixels wide. `sidebar_wanted` is already clamped to the
/// sidebar's own min and max. `outline_wanted` is the width the user set,
/// clamped here to 120–480. The panes keep about 240px while the outline
/// can shrink toward 120, then the sidebar shrinks toward its minimum.
/// The returned outline can be narrower than `outline_wanted`; the caller
/// keeps the wanted width and writes it only when the user drags.
pub(crate) fn column_widths(
    total: f32,
    sidebar_wanted: Option<f32>,
    outline_wanted: f32,
) -> (f32, f32) {
    let mut outline = outline_wanted.clamp(MIN_WIDTH, MAX_WIDTH);
    let mut sidebar = sidebar_wanted.unwrap_or(0.0);
    let gaps = GAP + if sidebar_wanted.is_some() { GAP } else { 0.0 };
    let panes_room = total - sidebar - outline - gaps;
    if panes_room < PANES_RESERVE {
        let need = PANES_RESERVE - panes_room;
        let from_outline = need.min((outline - MIN_WIDTH).max(0.0));
        outline -= from_outline;
        if sidebar_wanted.is_some() {
            sidebar = (sidebar - (need - from_outline)).max(crate::sidebar::MIN_WIDTH);
        }
    }
    let max_outline = (total - gaps - sidebar).max(0.0);
    (sidebar, outline.min(max_outline).max(0.0))
}

/// Draws the outline and returns the byte offset of a clicked heading.
/// `headings` is the cached list from [`Outline::refresh`].
pub(crate) fn show(ui: &mut egui::Ui, headings: &[Heading], caret: usize) -> Option<usize> {
    let colors = theme::current(ui.ctx());
    ui.painter()
        .rect_filled(ui.max_rect(), 0.0, colors.background);
    ui.add_space(4.0);
    ui.label(RichText::new("Outline").strong().color(colors.text));

    if headings.is_empty() {
        ui.label(RichText::new("No headings").color(colors.hint));
        return None;
    }

    let current = headings.iter().rposition(|h| h.offset <= caret);
    let mut clicked = None;
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .id_salt("outline")
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (index, heading) in headings.iter().enumerate() {
                if paint_row(ui, heading, current == Some(index), &colors)
                    .is_some_and(|r| r.clicked())
                {
                    clicked = Some(heading.offset);
                }
            }
        });
    clicked
}

fn paint_row(
    ui: &mut egui::Ui,
    heading: &Heading,
    current: bool,
    colors: &theme::Theme,
) -> Option<egui::Response> {
    let width = ui.available_width();
    if !width.is_finite() {
        return None;
    }
    let (rect, response) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::CLICK);
    if current {
        ui.painter().rect_filled(rect, 0.0, colors.current_row);
    }
    let indent = 8.0 + heading.level.saturating_sub(1) as f32 * INDENT;
    let color = if current { colors.heading } else { colors.text };
    let label = if heading.text.is_empty() {
        "Empty heading"
    } else {
        heading.text.as_str()
    };
    let font = FontId::proportional(13.0);
    let max_width = (rect.width() - indent - 8.0).max(0.0);
    let galley = elide(ui, label, font, color, max_width);
    let pos = pos2(
        rect.left() + indent,
        rect.center().y - galley.size().y * 0.5,
    );
    ui.painter().galley(pos, galley, color);
    Some(response)
}

/// One line, cut with … so a long heading stays inside the outline.
fn elide(
    ui: &egui::Ui,
    text: &str,
    font: FontId,
    color: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap.max_width = max_width.max(0.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.painter().layout_job(job)
}
