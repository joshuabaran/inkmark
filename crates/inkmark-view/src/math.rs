//! Raster formulas for the live pane. The source is never rewritten: a
//! stand-in gap is sized to the drawing, and the drawing is painted over it.
//! A formula that does not parse is reported back so the layout can show
//! the bytes instead.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use egui::{Color32, ColorImage, Context, Rect, TextureHandle, TextureOptions, Vec2, pos2, vec2};
use inkmark_text::{FontStyle, LineGeometry, RichLine, TextRenderer};
use latex_rust::{Color, Dim, MathFont, SvgOptions, latex_to_svg};

use crate::live_layout::{LeafLayout, Segment};

/// Inline formulas sit in the text row. Display formulas take a taller row.
const INLINE_OF_ROW: f32 = 0.9;
const DISPLAY_ROWS: f32 = 2.2;
const MAX_SIDE: u32 = 4096;

/// What the gap is sized against. The same values must be used to paint.
#[derive(Clone, Copy)]
pub(crate) struct Paint {
    pub wrap: f32,
    pub color: Color32,
    pub row: f32,
    pub ppp: f32,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    tex: String,
    display: bool,
    rgba: [u8; 4],
    target_px: u32,
    max_px: u32,
}

struct Slot {
    size: Vec2,
    image: Option<ColorImage>,
    texture: Option<TextureHandle>,
}

enum Cached {
    Ready(Slot),
    Failed,
}

#[derive(Default)]
pub(crate) struct MathCache {
    slots: HashMap<Key, Cached>,
}

impl MathCache {
    /// Widen each formula's stand-in to its drawing. Returns the source
    /// ranges that did not parse; those layouts should be built again with
    /// the bytes showing. Ranges only grow across calls.
    pub(crate) fn fit(
        &mut self,
        text: &mut TextRenderer,
        layout: &mut LeafLayout,
        paint: &Paint,
    ) -> Vec<std::ops::Range<usize>> {
        let mut failed = Vec::new();
        for seg in &mut layout.segments {
            let mut plan = Vec::new();
            for (i, math) in seg.maths.iter().enumerate() {
                let font = font_at(seg, math.display.start);
                match self.prepare(&math.tex, math.display_style, paint) {
                    Some(size) => {
                        let n = columns(text, font, size.x, paint.wrap);
                        plan.push((i, n));
                    }
                    None => failed.push(math.source.clone()),
                }
            }
            for (i, n) in plan {
                widen(seg, i, n);
            }
        }
        failed
    }

    /// Point size of a formula already prepared for `paint`, if it parsed.
    pub(crate) fn size(&self, tex: &str, display: bool, paint: &Paint) -> Option<Vec2> {
        match self.slots.get(&key(tex, display, paint))? {
            Cached::Ready(slot) => Some(slot.size),
            Cached::Failed => None,
        }
    }

    /// The formula's texture, uploaded on first use.
    pub(crate) fn texture(
        &mut self,
        ctx: &Context,
        tex: &str,
        display: bool,
        paint: &Paint,
    ) -> Option<(TextureHandle, Vec2)> {
        let key = key(tex, display, paint);
        let slot = match self.slots.get_mut(&key)? {
            Cached::Ready(slot) => slot,
            Cached::Failed => return None,
        };
        if slot.texture.is_none() {
            let image = slot.image.take()?;
            slot.texture = Some(ctx.load_texture(
                format!("inkmark-math-{}-{}", key.target_px, key.max_px),
                image,
                TextureOptions::LINEAR,
            ));
        }
        Some((slot.texture.clone()?, slot.size))
    }

    fn prepare(&mut self, tex: &str, display: bool, paint: &Paint) -> Option<Vec2> {
        let key = key(tex, display, paint);
        if let Some(cached) = self.slots.get(&key) {
            return match cached {
                Cached::Ready(slot) => Some(slot.size),
                Cached::Failed => None,
            };
        }
        let cached = match raster(tex, display, paint) {
            Some((image, size)) => Cached::Ready(Slot {
                size,
                image: Some(image),
                texture: None,
            }),
            None => Cached::Failed,
        };
        let size = match &cached {
            Cached::Ready(slot) => Some(slot.size),
            Cached::Failed => None,
        };
        self.slots.insert(key, cached);
        size
    }
}

fn key(tex: &str, display: bool, paint: &Paint) -> Key {
    let target_pt = if display {
        paint.row * DISPLAY_ROWS
    } else {
        paint.row * INLINE_OF_ROW
    };
    let ppp = paint.ppp.max(0.01);
    Key {
        tex: tex.to_owned(),
        display,
        rgba: paint.color.to_array(),
        target_px: (target_pt * ppp).round().max(1.0) as u32,
        max_px: (paint.wrap * ppp).round().clamp(1.0, MAX_SIDE as f32) as u32,
    }
}

fn font() -> Option<&'static MathFont> {
    static FONT: OnceLock<Option<MathFont>> = OnceLock::new();
    FONT.get_or_init(|| MathFont::stix_two_math().ok()).as_ref()
}

fn formula_svg(tex: &str, display: bool, color: Color32, font_pt: i64) -> Option<String> {
    let font = font()?;
    let options = SvgOptions {
        font_size_pt: Dim::from_i64(font_pt.max(1)),
        color: Color::rgb(color.r(), color.g(), color.b()),
        display,
    };
    latex_to_svg(tex, font, &options).ok()
}

fn raster(tex: &str, display: bool, paint: &Paint) -> Option<(ColorImage, Vec2)> {
    let spec = key(tex, display, paint);
    let target_pt = spec.target_px as f32 / paint.ppp.max(0.01);
    let svg = formula_svg(tex, display, paint.color, target_pt.round().max(8.0) as i64)?;
    let image = raster_svg(svg.as_bytes(), spec.target_px, spec.max_px)?;
    let ppp = paint.ppp.max(0.01);
    let size = vec2(image.width() as f32 / ppp, image.height() as f32 / ppp);
    Some((image, size))
}

fn raster_svg(data: &[u8], target_h: u32, max_w: u32) -> Option<ColorImage> {
    use resvg::{tiny_skia, usvg};

    let options = usvg::Options {
        fontdb: Arc::new(usvg::fontdb::Database::new()),
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(data, &options).ok()?;
    let size = tree.size();
    if size.width() <= 0.0 || size.height() <= 0.0 {
        return None;
    }
    let mut scale = target_h.max(1) as f32 / size.height();
    let mut w = (size.width() * scale).ceil().max(1.0);
    let max_w = max_w.max(1) as f32;
    if w > max_w {
        scale *= max_w / w;
        w = max_w;
    }
    let mut h = (size.height() * scale).ceil().max(1.0);
    let cap = MAX_SIDE as f32;
    if w > cap || h > cap {
        let k = cap / w.max(h);
        scale *= k;
        w = (size.width() * scale).ceil().max(1.0);
        h = (size.height() * scale).ceil().max(1.0);
    }
    let (w, h) = (w as u32, h as u32);
    let mut pixmap = tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some(ColorImage::from_rgba_premultiplied(
        [w as usize, h as usize],
        pixmap.data(),
    ))
}

fn font_at(seg: &Segment, at: usize) -> Option<FontStyle> {
    seg.runs
        .iter()
        .find(|(r, _)| r.start <= at && at < r.end)
        .map(|(_, font)| *font)
}

fn columns(text: &mut TextRenderer, font: Option<FontStyle>, width: f32, wrap: f32) -> usize {
    let adv = m_advance(text, font).max(0.5);
    let fit = (wrap / adv).floor().max(1.0) as usize;
    let mut n = (width / adv).ceil().max(1.0) as usize;
    n = n.min(fit).max(1);
    // `M` does not break against `M`, so a run no wider than the wrap stays
    // on one line. Hinting can still land a hair over; drop a column then.
    while n > 1 && !one_line(text, font, n, wrap) {
        n -= 1;
    }
    n
}

fn m_advance(text: &mut TextRenderer, font: Option<FontStyle>) -> f32 {
    let g = shape(text, "M", font, None);
    g.caret_x(0, 1)
}

fn one_line(text: &mut TextRenderer, font: Option<FontStyle>, n: usize, wrap: f32) -> bool {
    let sample = "M".repeat(n);
    let g = shape(text, &sample, font, Some(wrap));
    let width = g
        .rows
        .first()
        .and_then(|r| r.clusters.last().map(|c| c.x + c.w))
        .unwrap_or(0.0);
    g.rows.len() == 1 && width <= wrap + 0.5
}

fn shape(
    text: &mut TextRenderer,
    sample: &str,
    font: Option<FontStyle>,
    wrap: Option<f32>,
) -> LineGeometry {
    let runs = font
        .map(|font| vec![(0..sample.len(), font)])
        .unwrap_or_default();
    text.rich_geometry(RichLine {
        text: sample,
        runs: &runs,
        wrap_width: wrap,
    })
}

fn widen(seg: &mut Segment, index: usize, n: usize) {
    let at = seg.maths[index].display.start;
    let old_len = seg.maths[index].display.end - at;
    seg.text.replace_range(at..at + old_len, &"M".repeat(n));
    seg.maths[index].display.end = at + n;
    for math in &mut seg.maths[index + 1..] {
        retarget(&mut math.display, at, old_len, n);
    }
    for piece in &mut seg.pieces {
        retarget(&mut piece.display, at, old_len, n);
    }
    for (range, _) in &mut seg.runs {
        retarget(range, at, old_len, n);
    }
    for (range, _) in &mut seg.colors {
        retarget(range, at, old_len, n);
    }
    for range in &mut seg.strikes {
        retarget(range, at, old_len, n);
    }
}

fn retarget(range: &mut std::ops::Range<usize>, at: usize, old_len: usize, new_len: usize) {
    let old_end = at + old_len;
    let shift = new_len as isize - old_len as isize;
    if range.end <= at || shift == 0 {
        return;
    }
    if range.start >= old_end {
        range.start = (range.start as isize + shift) as usize;
        range.end = (range.end as isize + shift) as usize;
        return;
    }
    if range.start > at {
        range.start = (range.start as isize + shift) as usize;
    }
    range.end = (range.end as isize + shift) as usize;
}

/// Where to paint a formula: centered in the stand-in clusters.
pub(crate) fn formula_rect(
    geo: &LineGeometry,
    display: std::ops::Range<usize>,
    size: Vec2,
) -> Option<Rect> {
    let mut union: Option<Rect> = None;
    for row in &geo.rows {
        for cluster in &row.clusters {
            if cluster.end > display.start && cluster.start < display.end {
                let rect =
                    Rect::from_min_size(pos2(cluster.x, row.top), vec2(cluster.w, row.height));
                union = Some(union.map_or(rect, |u| u.union(rect)));
            }
        }
    }
    let union = union?;
    let w = size.x.min(union.width()).max(1.0);
    let h = size.y.min(union.height()).max(1.0);
    Some(Rect::from_center_size(union.center(), vec2(w, h)))
}

/// A click on the stand-in's ink enters the formula at its first byte.
pub(crate) fn formula_hit(seg: &Segment, geo: &LineGeometry, x: f32, d: usize) -> Option<usize> {
    for math in &seg.maths {
        let in_gap = math.display.start <= d && d < math.display.end;
        let on_ink = geo.rows.iter().any(|row| {
            row.clusters.iter().any(|c| {
                c.end > math.display.start
                    && c.start < math.display.end
                    && x >= c.x
                    && x < c.x + c.w
            })
        });
        if in_gap || on_ink {
            return Some(math.source.start);
        }
    }
    None
}

/// Grow a display-math row to the drawing and center that drawing.
pub(crate) fn raise_display(geo: &mut LineGeometry, size: Vec2, wrap: f32) {
    let extra = size.y - geo.height();
    if extra > 0.0
        && let Some(last) = geo.rows.last_mut()
    {
        last.height += extra;
    }
    if let Some(row) = geo.rows.first_mut() {
        let used = row.clusters.last().map(|c| c.x + c.w).unwrap_or(0.0);
        let dx = ((wrap - used) * 0.5).max(0.0);
        if dx > 0.0 {
            for cluster in &mut row.clusters {
                cluster.x += dx;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fraction_rasterizes_in_the_text_color_and_bad_latex_does_not() {
        let svg = formula_svg("x", false, Color32::from_rgb(10, 20, 30), 16).expect("x");
        assert!(svg.contains("#0a141e"), "{svg}");
        let paint = Paint {
            wrap: 400.0,
            color: Color32::BLACK,
            row: 26.0,
            ppp: 1.0,
        };
        let (image, size) = raster(r"\frac{1}{2}", true, &paint).expect("fraction");
        assert!(image.width() > 1 && image.height() > 1);
        assert!(size.x > 1.0 && size.y > 1.0);
        assert!(image.pixels.iter().any(|p| p.a() > 0));
        assert!(raster(r"\frac{", false, &paint).is_none());
    }
}
