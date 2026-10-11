//! The tab strip above both panes. The editor hides it when one tab is open.

use eframe::egui::{self, FontId, Sense, Stroke, pos2, vec2};
use inkmark_view::theme;

/// Height of the strip, including its bottom border.
pub(crate) const HEIGHT: f32 = 28.0;

pub(crate) struct Item {
    /// Stable for the life of the tab. The chip's egui id includes it.
    pub id: u64,
    pub title: String,
    pub dirty: bool,
    pub active: bool,
    /// Italic title. T3 sets this; until then every tab is pinned.
    pub preview: bool,
}

pub(crate) enum Event {
    Activate(usize),
    Close(usize),
}

/// Draws one chip per tab. A click activates it. A middle click closes it.
/// The chips are not focusable, so the key that switched tabs can hand
/// focus back to the document.
pub(crate) fn show(ui: &mut egui::Ui, tabs: &[Item]) -> Vec<Event> {
    let colors = theme::current(ui.ctx());
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, 0.0, colors.background);
    ui.painter().hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0, colors.divider),
    );
    let mut events = Vec::new();
    egui::ScrollArea::horizontal()
        .id_salt("tab_strip")
        .auto_shrink([false, false])
        .scroll_bar_visibility(egui::containers::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (index, tab) in tabs.iter().enumerate() {
                    if let Some(event) = chip(ui, index, tab) {
                        events.push(event);
                    }
                }
            });
        });
    events
}

fn chip(ui: &mut egui::Ui, index: usize, tab: &Item) -> Option<Event> {
    let colors = theme::current(ui.ctx());
    let font = FontId::proportional(13.0);
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &tab.title,
        0.0,
        egui::TextFormat {
            font_id: font.clone(),
            color: colors.text,
            italics: tab.preview,
            ..Default::default()
        },
    );
    if tab.dirty {
        job.append(
            " ●",
            0.0,
            egui::TextFormat {
                font_id: font,
                color: colors.text,
                ..Default::default()
            },
        );
    }
    let galley = ui.painter().layout_job(job);
    let width = galley.size().x + 20.0;
    let (_, rect) = ui.allocate_space(vec2(width, HEIGHT));
    let response = ui.interact(rect, egui::Id::new(("tab_chip", tab.id)), Sense::click());
    let fill = if tab.active {
        Some(colors.selection)
    } else if response.hovered() {
        Some(colors.current_row)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, 0.0, fill);
    }
    if tab.active {
        ui.painter().hline(
            rect.x_range(),
            rect.bottom() - 1.0,
            Stroke::new(2.0, colors.caret),
        );
    }
    ui.painter().vline(
        rect.right(),
        rect.y_range(),
        Stroke::new(1.0, colors.divider),
    );
    let pos = pos2(rect.left() + 10.0, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(pos, galley, colors.text);
    if response.middle_clicked() {
        Some(Event::Close(index))
    } else if response.clicked() {
        Some(Event::Activate(index))
    } else {
        None
    }
}
