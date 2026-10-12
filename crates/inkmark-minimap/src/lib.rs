//! Minimap sampling and paint helpers.
//!
//! A minimap draws a pane's document at a fixed small scale. When the
//! document is taller than the minimap can show, the minimap scrolls in
//! proportion to the pane (like VS Code's), so only a window of the document
//! is ever sampled: cost per frame depends on the minimap's height, not the
//! document's.
//!
//! Dragging the preview box keeps that box under the pointer. A short
//! document scrolls `dy / SCALE` points for a pointer move of `dy`. A taller
//! one keeps the same box tracking, and the window follows the scroll in
//! proportion, so both ends of the document stay reachable.

use egui::{Color32, Painter, Rect, Stroke, StrokeKind, pos2};

/// Minimap width in points.
pub const WIDTH: f32 = 88.0;
/// Document points per minimap point is `1 / SCALE`.
pub const SCALE: f32 = 0.125;

/// Where a minimap sits and what part of the document it shows this frame.
/// Document coordinates are the pane's own (its line heights), in points.
#[derive(Clone, Copy, Debug)]
pub struct Minimap {
    pub rect: Rect,
    /// Height of the whole document.
    total: f64,
    /// Height of the pane's visible area.
    viewport: f64,
    /// Document y at the top of the pane's visible area.
    view_top: f64,
}

impl Minimap {
    pub fn new(rect: Rect, total: f64, viewport: f32, view_top: f64) -> Self {
        Self {
            rect,
            total: total.max(0.0),
            viewport: f64::from(viewport),
            view_top,
        }
    }

    /// How much document fits in the minimap at once.
    pub fn window_height(&self) -> f64 {
        f64::from(self.rect.height()) / f64::from(SCALE)
    }

    /// Document y at the top of the minimap.
    pub fn window_top(&self) -> f64 {
        self.window_top_at(self.view_top)
    }

    /// The same window, if the pane were scrolled to `view_top`.
    fn window_top_at(&self, view_top: f64) -> f64 {
        let window = self.window_height();
        let max_top = (self.total - self.viewport).max(0.0);
        if self.total <= window || self.total <= self.viewport || max_top <= 0.0 {
            return 0.0;
        }
        let y = view_top.clamp(0.0, max_top);
        (y / max_top) * (self.total - window)
    }

    fn with_view_top(self, view_top: f64) -> Self {
        Self { view_top, ..self }
    }

    /// The document range the minimap shows: sample these lines/blocks.
    pub fn window(&self) -> (f64, f64) {
        let top = self.window_top();
        (top, top + self.window_height())
    }

    pub fn to_screen(&self, doc_y: f64) -> f32 {
        self.rect.top() + ((doc_y - self.window_top()) * f64::from(SCALE)) as f32
    }

    pub fn to_doc(&self, screen_y: f32) -> f64 {
        self.window_top() + f64::from(screen_y - self.rect.top()) / f64::from(SCALE)
    }

    /// The pane's visible area, drawn on the minimap.
    pub fn viewport_rect(&self) -> Rect {
        let top = self.to_screen(self.view_top);
        let bottom = self.to_screen(self.view_top + self.viewport);
        Rect::from_x_y_ranges(self.rect.x_range(), top..=bottom).intersect(self.rect)
    }

    /// New pane top for a click at `screen_y`: centre the view there.
    pub fn jump_target(&self, screen_y: f32) -> f64 {
        self.clamp_top(self.to_doc(screen_y) - self.viewport / 2.0)
    }

    /// How far `screen_y` sits below the preview box's top, in screen points.
    /// A press on the box records this and does not scroll.
    pub fn grab(&self, screen_y: f32) -> f32 {
        screen_y - self.viewport_rect().top()
    }

    /// Grab offset after [`jump_target`](Self::jump_target) has centred on
    /// `screen_y`, so a drag from that click keeps the pointer on the box
    /// the jump produced.
    pub fn grab_after_jump(&self, screen_y: f32) -> f32 {
        self.with_view_top(self.jump_target(screen_y))
            .grab(screen_y)
    }

    /// Pane top that puts the preview box `offset` screen points below
    /// `screen_y`. `offset` comes from [`grab`](Self::grab).
    ///
    /// The box top moves with the pointer until the document ends. A short
    /// document (the minimap window stays put) scrolls `dy / SCALE` document
    /// points. A taller one uses the same screen tracking: the window moves
    /// in proportion to the scroll, which is what keeps the box under the
    /// pointer while the whole document stays reachable.
    pub fn drag_to(&self, screen_y: f32, offset: f32) -> f64 {
        let desired = screen_y - offset;
        let max_top = (self.total - self.viewport).max(0.0);
        if max_top <= 0.0 {
            return 0.0;
        }
        // Already there: don't nudge the pane for a pointer that hasn't moved.
        if (desired - self.viewport_rect().top()).abs() <= f32::EPSILON {
            return self.clamp_top(self.view_top);
        }
        let scale = f64::from(SCALE);
        let from_top = f64::from(desired - self.rect.top());
        let window = self.window_height();
        let y = if self.total <= window || self.total <= self.viewport {
            from_top / scale
        } else {
            // Screen travel of the box top: the minimap, minus the box.
            let travel = f64::from(self.rect.height()) - self.viewport * scale;
            if travel < 1.0 {
                let frac = (from_top / f64::from(self.rect.height())).clamp(0.0, 1.0);
                return self.clamp_top(frac * max_top);
            }
            (from_top / travel) * max_top
        };
        self.clamp_top(y)
    }

    fn clamp_top(&self, top: f64) -> f64 {
        top.clamp(0.0, (self.total - self.viewport).max(0.0))
    }

    pub fn paint_background(&self, painter: &Painter, color: Color32) {
        painter.rect_filled(self.rect, 0.0, color);
    }

    /// A horizontal bar for document range `doc_y .. doc_y + height`,
    /// spanning `x` (fractions of the minimap width). At least one point tall.
    pub fn bar(&self, painter: &Painter, doc_y: f64, height: f64, x: (f32, f32), color: Color32) {
        let top = self.to_screen(doc_y);
        let h = ((height * f64::from(SCALE)) as f32).max(1.0);
        if top > self.rect.bottom() || top + h < self.rect.top() {
            return;
        }
        let left = self.rect.left() + 4.0;
        let width = self.rect.width() - 8.0;
        let r = Rect::from_min_max(
            pos2(left + width * x.0, top),
            pos2(left + width * x.1.max(x.0), top + h),
        )
        .intersect(self.rect);
        painter.rect_filled(r, 0.0, color);
    }

    /// The visible window: `fill` (darker or lighter while `hovered`, the
    /// caller picks) and an `edge` outline.
    pub fn paint_viewport(&self, painter: &Painter, fill: Color32, edge: Color32) {
        let r = self.viewport_rect();
        painter.rect_filled(r, 0.0, fill);
        painter.rect_stroke(r, 0.0, Stroke::new(1.0, edge), StrokeKind::Inside);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(total: f64, view_top: f64) -> Minimap {
        // 400 pt tall minimap shows 3200 pt of document; pane shows 800.
        Minimap::new(
            Rect::from_min_max(pos2(0.0, 100.0), pos2(88.0, 500.0)),
            total,
            800.0,
            view_top,
        )
    }

    #[test]
    fn short_documents_fit_without_scrolling() {
        let m = map(2000.0, 600.0);
        assert_eq!(m.window_top(), 0.0);
        assert_eq!(m.to_screen(0.0), 100.0);
        assert_eq!(m.to_screen(800.0), 200.0);
        let v = m.viewport_rect();
        assert_eq!((v.top(), v.bottom()), (175.0, 275.0));
    }

    #[test]
    fn long_documents_scroll_the_minimap_proportionally() {
        let total = 100_000.0;
        assert_eq!(map(total, 0.0).window_top(), 0.0);
        let end = map(total, total - 800.0);
        assert_eq!(end.window_top(), total - 3200.0);
        // The viewport box sits at the bottom of the minimap at the end.
        assert!((end.viewport_rect().bottom() - 500.0).abs() < 1e-3);
        let mid = map(total, (total - 800.0) / 2.0);
        assert!((mid.window_top() - (total - 3200.0) / 2.0).abs() < 1e-6);
        // to_doc inverts to_screen.
        for y in [100.0, 250.0, 499.0] {
            assert!((f64::from(mid.to_screen(mid.to_doc(y))) - f64::from(y)).abs() < 1e-3);
        }
    }

    #[test]
    fn clicks_outside_the_box_centre_on_the_pointer() {
        let m = map(100_000.0, 0.0);
        // Click 50 pt into the minimap = 400 pt into the document; centred.
        assert_eq!(m.jump_target(150.0), 0.0);
        assert_eq!(m.jump_target(300.0), 1600.0 - 400.0);
    }

    /// Box top after scrolling this minimap to `top`.
    fn box_at(m: Minimap, top: f64) -> f32 {
        m.with_view_top(top).viewport_rect().top()
    }

    #[test]
    fn a_press_on_the_box_does_not_move_it_and_a_drag_tracks_the_pointer() {
        // Short: the window stays at the top. 40 screen points is 320 document points.
        let short = map(2000.0, 600.0);
        let on_box = 200.0;
        let offset = short.grab(on_box);
        assert!((short.drag_to(on_box, offset) - 600.0).abs() < 1e-6);
        let dragged = short.drag_to(on_box + 40.0, offset);
        assert!((dragged - 920.0).abs() < 1e-6);
        assert!((box_at(short, dragged) - (box_at(short, 600.0) + 40.0)).abs() < 1e-2);

        // Long: the window scrolls with the pane, and the box still moves by dy.
        let long = map(100_000.0, 20_000.0);
        let preview = long.viewport_rect().top();
        let held = long.grab(preview + 10.0);
        assert!((long.drag_to(preview + 10.0, held) - 20_000.0).abs() < 1e-3);
        let moved = long.drag_to(preview + 10.0 + 50.0, held);
        assert!(
            (box_at(long, moved) - (preview + 50.0)).abs() < 1e-2,
            "box {} after a 50 pt drag, from {preview}",
            box_at(long, moved)
        );
        assert!(moved > 20_000.0);

        // Both ends clamp, for a short document and a long one.
        for m in [short, long] {
            let offset = m.grab(m.viewport_rect().top() + 8.0);
            assert_eq!(m.drag_to(-10_000.0, offset), 0.0);
            assert_eq!(m.drag_to(10_000.0, offset), m.total - m.viewport);
            assert!((box_at(m, 0.0) - m.rect.top()).abs() < 1e-2);
        }
    }
}
