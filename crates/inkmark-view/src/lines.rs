//! Per-source-line heights and the scroll anchor, kept in step with the
//! document. Both panes index by source line, which makes scroll sync a
//! matter of sharing a line number.

use egui::{Id, Painter, Rect, Sense, Ui, pos2, vec2};
use inkmark_buffer::{Change, Document};
use inkmark_text::{HeightCache, ScrollAnchor};

use crate::theme::{SCROLL_THUMB, SCROLL_TRACK};

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
    synced_epoch: Option<u64>,
}

impl LineIndex {
    pub fn new() -> Self {
        Self {
            heights: HeightCache::new([]),
            anchor: ScrollAnchor::default(),
            synced_epoch: None,
        }
    }

    /// Re-estimate everything on the next sync (e.g. the font changed).
    pub fn invalidate(&mut self) {
        self.synced_epoch = None;
    }

    /// Forget the document entirely.
    pub fn reset(&mut self) {
        self.synced_epoch = None;
        self.anchor = ScrollAnchor::default();
    }

    /// Brings heights up to date with `doc`. `estimate` guesses a line's
    /// height from its length in chars.
    pub fn sync(&mut self, doc: &Document, estimate: impl Fn(usize) -> f32) -> Synced {
        let Some(since) = self.synced_epoch else {
            return self.rebuild(doc, estimate);
        };
        if since == doc.epoch() {
            return Synced::Unchanged;
        }
        let Some(changes) = doc.log().changes_since(since) else {
            return self.rebuild(doc, estimate);
        };
        let changes: Vec<Change> = changes.copied().collect();
        for c in &changes {
            let lines = c.lines;
            let old = lines.start..lines.start + lines.removed + 1;
            if old.end > self.heights.len() {
                return self.rebuild(doc, estimate);
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
        if self.heights.len() != doc.line_count() {
            return self.rebuild(doc, estimate);
        }
        self.synced_epoch = Some(doc.epoch());
        Synced::Changed(changes)
    }

    /// Wheel (when `hovered`) and scrollbar input for a pane whose
    /// scrollbar is `bar`. Returns whether it scrolled.
    pub fn scroll_input(&mut self, ui: &Ui, id: Id, hovered: bool, bar: Rect) -> bool {
        let viewport = bar.height();
        let mut scrolled = false;
        let bar_response = ui.interact(bar, id.with("scrollbar"), Sense::click_and_drag());
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

    pub fn paint_scrollbar(&self, painter: &Painter, bar: Rect) {
        painter.rect_filled(bar, 0.0, SCROLL_TRACK);
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
        painter.rect_filled(thumb, 3.0, SCROLL_THUMB);
    }

    fn rebuild(&mut self, doc: &Document, estimate: impl Fn(usize) -> f32) -> Synced {
        self.heights
            .reset_estimates(doc.rope().lines().map(|l| estimate(l.len_chars())));
        self.anchor.line = self.anchor.line.min(self.heights.len().saturating_sub(1));
        self.synced_epoch = Some(doc.epoch());
        Synced::Rebuilt
    }
}
