//! Per-source-line heights and the scroll anchor, kept in step with the
//! document. Both panes index by source line, which makes scroll sync a
//! matter of sharing a line number.

use egui::{Id, Painter, Rect, Sense, Ui, pos2, vec2};
use inkmark_buffer::{Change, Document};
use inkmark_minimap::Minimap;
use inkmark_text::{HeightCache, ScrollAnchor};

use crate::theme::Theme;

pub(crate) const SCROLLBAR_WIDTH: f32 = 10.0;

/// A scroll position both panes understand: `frac` of the way through
/// source line `line`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollPos {
    pub line: usize,
    pub frac: f32,
}

pub(crate) enum Synced {
    Unchanged,
    /// Changes since the last sync, in order (for mapping selections).
    Changed(Vec<Change>),
    /// Heights were rebuilt from scratch; offsets can't be mapped.
    Rebuilt,
}

pub(crate) struct LineIndex {
    pub heights: HeightCache,
    pub anchor: ScrollAnchor,
    /// Epoch the anchor (and the caller's selection) were last mapped to.
    synced_epoch: Option<u64>,
    /// Heights must be re-estimated (layout changed), but edits since
    /// `synced_epoch` still need mapping first.
    heights_stale: bool,
}

impl LineIndex {
    pub fn new() -> Self {
        Self {
            heights: HeightCache::new([]),
            anchor: ScrollAnchor::default(),
            synced_epoch: None,
            heights_stale: true,
        }
    }

    /// Re-estimate every height on the next sync (e.g. the font or width
    /// changed). Edits made since the last sync are still mapped.
    pub fn invalidate(&mut self) {
        self.heights_stale = true;
    }

    /// Forget the document entirely.
    pub fn reset(&mut self) {
        self.synced_epoch = None;
        self.heights_stale = true;
        self.anchor = ScrollAnchor::default();
    }

    /// Brings heights and the anchor up to date with `doc`. `estimate`
    /// guesses a line's height from its length in chars.
    pub fn sync(&mut self, doc: &Document, estimate: impl Fn(usize) -> f32) -> Synced {
        let changes: Option<Vec<Change>> = match self.synced_epoch {
            Some(since) if since == doc.epoch() => Some(Vec::new()),
            Some(since) => doc.log().changes_since(since).map(|c| c.copied().collect()),
            None => None,
        };
        let Some(changes) = changes else {
            return self.rebuild(doc, estimate, Vec::new());
        };
        if self.heights_stale {
            // Map the anchor through the edits even though heights are
            // rebuilt: a pane that was hidden must land where it was.
            for c in &changes {
                self.shift_anchor(c);
            }
            return self.rebuild(doc, estimate, changes);
        }
        if changes.is_empty() {
            return Synced::Unchanged;
        }
        for c in &changes {
            let lines = c.lines;
            let old = lines.start..lines.start + lines.removed + 1;
            if old.end > self.heights.len() {
                return self.rebuild(doc, estimate, Vec::new());
            }
            // Estimates only; real heights arrive when the lines are drawn.
            let estimates: Vec<f32> = (lines.start..lines.start + lines.inserted + 1)
                .map(|l| {
                    let chars = if l < doc.line_count() {
                        doc.rope().line(l).len_chars()
                    } else {
                        0
                    };
                    estimate(chars)
                })
                .collect();
            self.heights.splice(old, estimates.into_iter());
            self.shift_anchor(c);
        }
        if self.heights.len() != doc.line_count() {
            return self.rebuild(doc, estimate, Vec::new());
        }
        self.synced_epoch = Some(doc.epoch());
        Synced::Changed(changes)
    }

    /// Moves the anchor through one edit's line changes.
    fn shift_anchor(&mut self, c: &Change) {
        let lines = c.lines;
        if self.anchor.line > lines.start {
            if self.anchor.line <= lines.start + lines.removed {
                self.anchor = ScrollAnchor {
                    line: lines.start,
                    offset: 0.0,
                };
            } else {
                self.anchor.line = self.anchor.line - lines.removed + lines.inserted;
            }
        }
    }

    /// Wheel (when `hovered`) and scrollbar input for a pane whose
    /// scrollbar is `bar`. Returns whether it scrolled.
    pub fn scroll_input(&mut self, ui: &Ui, id: Id, hovered: bool, bar: Rect) -> bool {
        let viewport = bar.height();
        let mut scrolled = false;
        let bar_response = ui.interact(bar, id.with("scrollbar"), Sense::click_and_drag());
        keep_focus(ui, id, &bar_response);
        if (bar_response.dragged() || bar_response.clicked())
            && let Some(pos) = bar_response.interact_pointer_pos()
        {
            let frac = ((pos.y - bar.top()) / bar.height()).clamp(0.0, 1.0);
            let target = f64::from(frac) * self.heights.total() - f64::from(viewport) / 2.0;
            self.anchor = self.heights.line_at(target.max(0.0));
            self.anchor = self.heights.scroll_by(self.anchor, 0.0, viewport);
            scrolled = true;
        }
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if (hovered || bar_response.hovered()) && wheel != 0.0 {
            self.anchor = self.heights.scroll_by(self.anchor, -wheel, viewport);
            scrolled = true;
        }
        scrolled
    }

    /// The minimap for this pane at `rect`, as of this frame's scroll position.
    pub fn minimap(&self, rect: Rect, viewport: f32) -> Minimap {
        Minimap::new(
            rect,
            self.heights.total(),
            viewport,
            self.heights.anchor_y(self.anchor),
        )
    }

    /// Click (jump, centred) and drag (scrollbar-like) on a minimap at
    /// `rect`. Returns (scrolled, hovered).
    pub fn minimap_input(&mut self, ui: &Ui, id: Id, rect: Rect, viewport: f32) -> (bool, bool) {
        let response = ui.interact(rect, id.with("minimap"), Sense::click_and_drag());
        keep_focus(ui, id, &response);
        let m = self.minimap(rect, viewport);
        let pressed =
            response.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed());
        let target = match response.interact_pointer_pos() {
            Some(p) if pressed => Some(m.jump_target(p.y)),
            Some(p) if response.dragged() => Some(m.drag_target(p.y)),
            _ => None,
        };
        if let Some(top) = target {
            self.anchor = self.heights.line_at(top);
        }
        (target.is_some(), response.hovered())
    }

    pub fn paint_scrollbar(&self, painter: &Painter, bar: Rect, theme: &Theme) {
        painter.rect_filled(bar, 0.0, theme.scroll_track);
        let total = self.heights.total().max(1.0) as f32;
        let viewport = bar.height();
        if total <= viewport {
            return;
        }
        let height = (viewport / total * bar.height()).max(24.0);
        let top = self.heights.anchor_y(self.anchor) as f32 / (total - viewport)
            * (bar.height() - height);
        let thumb = Rect::from_min_size(
            pos2(bar.left() + 2.0, bar.top() + top),
            vec2(SCROLLBAR_WIDTH - 4.0, height),
        );
        painter.rect_filled(thumb, 3.0, theme.scroll_thumb);
    }

    /// Re-estimates every height. `changes` (already applied to the anchor)
    /// are passed on so callers can still map their selection.
    fn rebuild(
        &mut self,
        doc: &Document,
        estimate: impl Fn(usize) -> f32,
        changes: Vec<Change>,
    ) -> Synced {
        self.heights
            .reset_estimates(doc.rope().lines().map(|l| estimate(l.len_chars())));
        self.anchor.line = self.anchor.line.min(self.heights.len().saturating_sub(1));
        self.synced_epoch = Some(doc.epoch());
        self.heights_stale = false;
        if changes.is_empty() {
            Synced::Rebuilt
        } else {
            Synced::Changed(changes)
        }
    }
}

/// Clicking a pane's scrollbar or minimap keeps (or gives) keyboard focus to
/// the pane's text, so typing carries on where the caret is.
fn keep_focus(ui: &Ui, id: Id, response: &egui::Response) {
    // egui drops focus on a click (the release) outside the focused widget,
    // so reclaim it on press, click and drag end alike.
    if response.is_pointer_button_down_on() || response.clicked() || response.drag_stopped() {
        ui.memory_mut(|m| m.request_focus(id));
    }
}
