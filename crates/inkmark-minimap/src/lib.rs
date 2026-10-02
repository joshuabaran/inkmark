//! Minimap sampling and paint helpers.
//!
//! A minimap draws a pane's document at a fixed small scale. When the
//! document is taller than the minimap can show, the minimap scrolls in
//! proportion to the pane (like VS Code's), so only a window of the document
//! is ever sampled: cost per frame depends on the minimap's height, not the
//! document's.

use egui::{Color32, Painter, Rect, Stroke, StrokeKind, pos2};

/// Minimap width in points.
pub const WIDTH: f32 = 88.0;
/// Document points per minimap point is `1 / SCALE`.
pub const SCALE: f32 = 0.125;

const BACKGROUND: Color32 = Color32::from_rgb(19, 19, 23);
const VIEWPORT: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);
const VIEWPORT_HOVER: Color32 = Color32::from_rgba_premultiplied(30, 30, 30, 30);
const VIEWPORT_EDGE: Color32 = Color32::from_rgba_premultiplied(40, 40, 40, 40);

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
        let window = self.window_height();
        if self.total <= window || self.total <= self.viewport {
            return 0.0;
        }
        let progress = (self.view_top / (self.total - self.viewport)).clamp(0.0, 1.0);
        progress * (self.total - window)
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

    /// New pane top while dragging: the minimap acts like a scrollbar track.
    pub fn drag_target(&self, screen_y: f32) -> f64 {
        let frac = f64::from((screen_y - self.rect.top()) / self.rect.height()).clamp(0.0, 1.0);
        self.clamp_top(frac * (self.total - self.viewport))
    }

    fn clamp_top(&self, top: f64) -> f64 {
        top.clamp(0.0, (self.total - self.viewport).max(0.0))
    }

    pub fn paint_background(&self, painter: &Painter) {
        painter.rect_filled(self.rect, 0.0, BACKGROUND);
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

    pub fn paint_viewport(&self, painter: &Painter, hovered: bool) {
        let r = self.viewport_rect();
        painter.rect_filled(r, 0.0, if hovered { VIEWPORT_HOVER } else { VIEWPORT });
        painter.rect_stroke(r, 0.0, Stroke::new(1.0, VIEWPORT_EDGE), StrokeKind::Inside);
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
    fn clicks_centre_and_drags_track_like_a_scrollbar() {
        let m = map(100_000.0, 0.0);
        // Click 50 pt into the minimap = 400 pt into the document; centred.
        assert_eq!(m.jump_target(150.0), 0.0);
        assert_eq!(m.jump_target(300.0), 1600.0 - 400.0);
        assert_eq!(m.drag_target(100.0), 0.0);
        assert_eq!(m.drag_target(500.0), 100_000.0 - 800.0);
        assert_eq!(m.drag_target(9999.0), 100_000.0 - 800.0);
    }
}
