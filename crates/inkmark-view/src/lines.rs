//! Per-source-line heights and the scroll anchor, kept in step with the
//! document. Both panes index by source line, which makes scroll sync a
//! matter of sharing a line number.

use egui::{Id, Painter, Rect, Response, Sense, Ui, pos2, vec2};
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
    /// Points of a discrete notch already applied. Later frames still emit
    /// egui's smoothed slices of that same notch; those are skipped.
    wheel_credit: f32,
}

impl LineIndex {
    pub fn new() -> Self {
        Self {
            heights: HeightCache::new([]),
            anchor: ScrollAnchor::default(),
            synced_epoch: None,
            heights_stale: true,
            wheel_credit: 0.0,
        }
    }

    /// Re-estimate every height on the next sync (e.g. the font or width
    /// changed). Edits made since the last sync are still mapped.
    pub fn invalidate(&mut self) {
        self.heights_stale = true;
    }

    /// Whether the heights were last synced to `doc` as it is now, so they
    /// still describe its lines (a stale mark aside).
    pub fn synced_to(&self, doc: &Document) -> bool {
        self.synced_epoch == Some(doc.epoch())
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
        let wheel = if hovered || bar_response.hovered() {
            self.wheel_y(ui)
        } else {
            0.0
        };
        if wheel != 0.0 {
            self.anchor = self.heights.scroll_by(self.anchor, -wheel, viewport);
            scrolled = true;
        }
        scrolled
    }

    /// Vertical wheel delta for this frame, in the same sign as
    /// `smooth_scroll_delta` (positive moves content down).
    ///
    /// A mouse notch arrives as one line unit and, on Wayland, a following
    /// end phase. egui smooths the line unit over later frames, then the
    /// end phase drops whatever is left, so a notch only moved one slice.
    /// The whole notch is applied on the event frame. Slices of that same
    /// notch are ignored. Trackpad points, which are already smooth, pass
    /// through.
    pub fn wheel_y(&mut self, ui: &Ui) -> f32 {
        let (notches, smooth) = ui.input(|input| {
            let notches = input
                .events
                .iter()
                .map(|event| match event {
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Line,
                        delta,
                        phase: egui::TouchPhase::Move,
                        ..
                    } => delta.y,
                    _ => 0.0,
                })
                .sum::<f32>();
            (notches, input.smooth_scroll_delta.y)
        });
        // A wheel stop arrives as a zero point delta. It carries no movement,
        // and applying the smoothed tail of the notch here scrolls backward.
        let zero_stop = notches == 0.0
            && ui.input(|input| {
                input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta,
                            phase: egui::TouchPhase::Move,
                            ..
                        } if delta.length_sq() < 1e-6
                    )
                })
            });
        if zero_stop {
            return 0.0;
        }
        if notches == 0.0 && smooth == 0.0 {
            self.wheel_credit = 0.0;
            return 0.0;
        }
        let mut delta = 0.0;
        if notches != 0.0 {
            let speed = ui
                .ctx()
                .options(|options| options.input_options.line_scroll_speed);
            let full = notches * speed;
            delta += full;
            self.wheel_credit = (self.wheel_credit + full.abs() - smooth.abs()).max(0.0);
        }
        if self.wheel_credit > 0.0 {
            self.wheel_credit = (self.wheel_credit - smooth.abs()).max(0.0);
        } else {
            delta += smooth;
        }
        delta
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

    /// Click outside the preview box jumps, centred on the click. A press on
    /// the box grabs it, and a drag keeps the box under the pointer.
    /// Returns (scrolled, hovered).
    pub fn minimap_input(&mut self, ui: &Ui, id: Id, rect: Rect, viewport: f32) -> (bool, bool) {
        let response = ui.interact(rect, id.with("minimap"), Sense::click_and_drag());
        keep_focus(ui, id, &response);
        let m = self.minimap(rect, viewport);
        let target = minimap_target(ui, response.id, &m, &response);
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

/// Keys a focused pane keeps for itself: Tab and the arrows edit its text
/// rather than move egui's focus to the next widget. Escape isn't kept.
const PANE_KEYS: egui::EventFilter = egui::EventFilter {
    tab: true,
    horizontal_arrows: true,
    vertical_arrows: true,
    escape: false,
};

/// Keeps Tab and the arrows in the pane `id` while it has focus (see
/// `PANE_KEYS`). egui reads the lock when a frame begins, so it has to be
/// in place by the end of each frame, after anything that requests focus:
/// a request starts the focus over, unlocked.
pub(crate) fn lock_focus(ui: &Ui, id: Id) {
    lock_focus_ctx(ui.ctx(), id);
}

/// Same lock as [`lock_focus`], for a request made after the pane has
/// already drawn. A modal under the pointer makes `interact` surrender
/// focus, so the overlay asks again once the panes have run.
pub(crate) fn lock_focus_ctx(ctx: &egui::Context, id: Id) {
    ctx.memory_mut(|m| m.set_focus_lock_filter(id, PANE_KEYS));
}

/// Where a minimap press or drag should put the pane's top.
///
/// A press on the preview box records how far the pointer sits below the
/// box and does not scroll. A press elsewhere jumps, centred on the click,
/// and the drag that follows keeps the pointer on the box that jump made.
/// The offset lives on the minimap widget until the button comes up.
pub(crate) fn minimap_target(ui: &Ui, id: Id, map: &Minimap, response: &Response) -> Option<f64> {
    let pressed = response.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed());
    let target = match response.interact_pointer_pos() {
        Some(p) if map.in_unpainted_column(p.y) => None,
        Some(p) if pressed => {
            let preview = map.viewport_rect();
            let on_box = p.y >= preview.top() && p.y <= preview.bottom();
            let (top, offset) = if on_box {
                (None, map.grab(p.y))
            } else {
                let top = map.jump_target(p.y);
                (Some(top), map.grab_after_jump(p.y))
            };
            ui.data_mut(|data| data.insert_temp(id, offset));
            top
        }
        Some(p) if response.dragged() => {
            let offset = ui
                .data(|data| data.get_temp::<f32>(id))
                .unwrap_or_else(|| map.grab(p.y));
            Some(map.drag_to(p.y, offset))
        }
        _ => None,
    };
    // A release can come without a pointer position. Drop the grab either way.
    if response.drag_stopped() {
        ui.data_mut(|data| {
            data.remove_temp::<f32>(id);
        });
    }
    target
}

/// Clicking a pane's scrollbar or minimap keeps (or gives) keyboard focus to
/// the pane's text, so typing carries on where the caret is.
pub(crate) fn keep_focus(ui: &Ui, id: Id, response: &egui::Response) {
    // egui drops focus on a click (the release) outside the focused widget,
    // so reclaim it on press, click and drag end alike. The request clears
    // the pane's key lock, so lock it again: otherwise the next arrow key
    // moves focus to the nearest widget that way (the minimap, say) and
    // typing after it is lost.
    if response.is_pointer_button_down_on() || response.clicked() || response.drag_stopped() {
        ui.memory_mut(|m| m.request_focus(id));
        lock_focus(ui, id);
    }
}
