use std::collections::HashMap;
use std::ops::Range;

use cosmic_text::{
    Attrs, AttrsList, BufferLine, Ellipsize, Family, FontSystem, Hinting, LineEnding, Shaping, Wrap,
};
use egui::{Color32, Mesh, Painter, Pos2, Rect, Shape, pos2, vec2};

use crate::atlas::{AtlasStats, GlyphAtlas};
use crate::geometry::{ClusterSpan, LineGeometry, Row};

const TAB_WIDTH: u16 = 4;
/// Laid-out lines kept between frames before unused ones are dropped.
const LINE_CACHE_CAPACITY: usize = 4096;

/// Layout settings for a pane. Sizes are in points; changing any of them
/// invalidates every cached layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextConfig {
    pub monospace: bool,
    pub font_size: f32,
    pub line_height: f32,
    /// Soft-wrap width, or `None` for no wrapping.
    pub wrap_width: Option<f32>,
}

struct CachedLine {
    line: BufferLine,
    last_used: u64,
}

/// Lays out lines with cosmic-text and paints them through the glyph atlas.
///
/// Everything is laid out in physical pixels with metrics hinting, then
/// converted to points at paint time, so glyphs land on the pixel grid at any
/// scale factor. Layouts are cached by line text, so inserting or deleting
/// lines doesn't invalidate the lines around them.
pub struct TextRenderer {
    font_system: FontSystem,
    atlas: GlyphAtlas,
    config: Option<TextConfig>,
    pixels_per_point: f32,
    /// Line height rounded to whole physical pixels.
    line_height_px: f32,
    /// Average advance of typical text, for height estimates.
    avg_advance_px: f32,
    lines: HashMap<String, CachedLine>,
    frame: u64,
}

/// One mesh per atlas page, filled by [`TextRenderer::draw_line`].
#[derive(Default)]
pub struct GlyphMeshes {
    meshes: Vec<Mesh>,
}

impl GlyphMeshes {
    fn mesh(&mut self, atlas: &GlyphAtlas, page: usize) -> &mut Mesh {
        while self.meshes.len() <= page {
            self.meshes
                .push(Mesh::with_texture(atlas.texture_id(self.meshes.len())));
        }
        &mut self.meshes[page]
    }

    pub fn vertex_count(&self) -> usize {
        self.meshes.iter().map(|m| m.vertices.len()).sum()
    }
}

impl TextRenderer {
    pub fn new(ctx: &egui::Context) -> Self {
        Self {
            font_system: FontSystem::new(),
            atlas: GlyphAtlas::new(ctx),
            config: None,
            pixels_per_point: 1.0,
            line_height_px: 0.0,
            avg_advance_px: 0.0,
            lines: HashMap::new(),
            frame: 0,
        }
    }

    /// Starts a frame. Returns `true` if layout was invalidated, in which case
    /// callers must re-estimate their line heights.
    pub fn begin_frame(&mut self, config: TextConfig, pixels_per_point: f32) -> bool {
        self.frame += 1;
        self.atlas.begin_frame();
        if self.config == Some(config) && self.pixels_per_point == pixels_per_point {
            return false;
        }
        self.config = Some(config);
        self.pixels_per_point = pixels_per_point;
        self.line_height_px = (config.line_height * pixels_per_point).round().max(1.0);
        self.lines.clear();
        self.avg_advance_px = self.measure_avg_advance();
        true
    }

    fn config(&self) -> TextConfig {
        self.config.expect("begin_frame not called")
    }

    fn attrs(&self) -> Attrs<'static> {
        let family = if self.config().monospace {
            Family::Monospace
        } else {
            Family::SansSerif
        };
        Attrs::new().family(family)
    }

    fn new_line(&self, text: &str) -> BufferLine {
        BufferLine::new(
            text,
            LineEnding::None,
            AttrsList::new(&self.attrs()),
            Shaping::Advanced,
        )
    }

    fn measure_avg_advance(&mut self) -> f32 {
        const SAMPLE: &str = "The quick brown fox jumps over the lazy dog, then naps; 0123456789.";
        let (config, ppp) = (self.config(), self.pixels_per_point);
        let mut line = self.new_line(SAMPLE);
        let width = layout(&mut line, &mut self.font_system, config, ppp, false, None)
            .iter()
            .map(|l| l.w)
            .sum::<f32>();
        width / SAMPLE.chars().count() as f32
    }

    /// Height of a line in points, guessed from its length without shaping.
    pub fn estimate_height(&self, char_count: usize) -> f32 {
        let rows = match self.config().wrap_width {
            Some(width) if char_count > 0 => {
                let per_row = (width * self.pixels_per_point / self.avg_advance_px)
                    .floor()
                    .max(1.0);
                (char_count as f32 / per_row).ceil()
            }
            _ => 1.0,
        };
        rows * self.line_height_px / self.pixels_per_point
    }

    /// One empty row's height in points.
    pub fn row_height(&self) -> f32 {
        self.line_height_px / self.pixels_per_point
    }

    /// Lays out `text` (cached) and returns its height in points.
    pub fn line_height(&mut self, text: &str) -> f32 {
        let rows = self
            .cached_line(text)
            .layout_opt()
            .map_or(1, |l| l.len().max(1));
        rows as f32 * self.row_height()
    }

    /// Caret and hit-test geometry of `text`, in points from its top-left.
    pub fn geometry(&mut self, text: &str) -> LineGeometry {
        let ppp = self.pixels_per_point;
        let line_height_px = self.line_height_px;
        let line = self.cached_line(text);
        let mut rows: Vec<Row> = Vec::new();
        for run in line.layout_runs(None, line_height_px) {
            let clusters: Vec<ClusterSpan> = run
                .glyphs
                .iter()
                .map(|g| ClusterSpan {
                    start: g.start,
                    end: g.end,
                    x: g.x / ppp,
                    w: g.w / ppp,
                })
                .collect();
            let prev_end = rows.last().map_or(0, |r| r.end);
            let start = clusters.iter().map(|c| c.start).min().unwrap_or(prev_end);
            let end = clusters.iter().map(|c| c.end).max().unwrap_or(prev_end);
            rows.push(Row {
                top: run.line_top / ppp,
                height: run.line_height / ppp,
                start,
                end,
                clusters,
            });
        }
        if rows.is_empty() {
            rows.push(Row {
                height: self.row_height(),
                ..Row::default()
            });
        }
        // The last row owns everything to the end of the line.
        if let Some(last) = rows.last_mut() {
            last.end = last.end.max(text.len());
        }
        LineGeometry { rows }
    }

    fn cached_line(&mut self, text: &str) -> &mut BufferLine {
        let (frame, config, ppp) = (self.frame, self.config(), self.pixels_per_point);
        // Snap glyphs from monospace fallback fonts (e.g. CJK) to whole cells.
        let mono_width = config.monospace.then_some(self.avg_advance_px);
        if !self.lines.contains_key(text) {
            let mut line = self.new_line(text);
            layout(
                &mut line,
                &mut self.font_system,
                config,
                ppp,
                true,
                mono_width,
            );
            self.lines.insert(
                text.to_owned(),
                CachedLine {
                    line,
                    last_used: frame,
                },
            );
        }
        let cached = self.lines.get_mut(text).expect("inserted above");
        cached.last_used = frame;
        &mut cached.line
    }

    /// Paints `text` with its top-left corner at `top_left` (points).
    pub fn draw_line(&mut self, out: &mut GlyphMeshes, text: &str, top_left: Pos2, color: Color32) {
        self.draw_line_colored(out, text, top_left, color, &[]);
    }

    /// Like [`draw_line`](Self::draw_line), with byte ranges of `text` drawn
    /// in other colors. `colors` must be sorted and non-overlapping. Colors
    /// apply at paint time, so recoloring never re-shapes.
    pub fn draw_line_colored(
        &mut self,
        out: &mut GlyphMeshes,
        text: &str,
        top_left: Pos2,
        color: Color32,
        colors: &[(Range<usize>, Color32)],
    ) {
        let ppp = self.pixels_per_point;
        let line_height_px = self.line_height_px;
        let origin = ((top_left.x * ppp).round(), (top_left.y * ppp).round());
        self.cached_line(text);
        let Self {
            font_system,
            atlas,
            lines,
            ..
        } = self;
        let line = &lines[text].line;
        for run in line.layout_runs(None, line_height_px) {
            for glyph in run.glyphs {
                let physical = glyph.physical((origin.0, origin.1 + run.line_y), 1.0);
                let Some(g) = atlas.get(font_system, physical.cache_key) else {
                    continue;
                };
                let min = pos2(
                    (physical.x + g.left) as f32 / ppp,
                    (physical.y - g.top) as f32 / ppp,
                );
                let rect = Rect::from_min_size(min, vec2(g.size[0] as f32, g.size[1] as f32) / ppp);
                let tint = if g.is_color {
                    Color32::WHITE
                } else {
                    // Glyphs come in visual order, so search rather than walk.
                    let i = colors.partition_point(|(r, _)| r.end <= glyph.start);
                    match colors.get(i) {
                        Some((r, c)) if r.start <= glyph.start => *c,
                        _ => color,
                    }
                };
                out.mesh(atlas, g.page).add_rect_with_uv(rect, g.uv, tint);
            }
        }
    }

    /// Uploads new glyphs, paints the meshes and trims the line cache.
    pub fn end_frame(&mut self, out: GlyphMeshes, painter: &Painter) {
        self.atlas.upload();
        painter.extend(
            out.meshes
                .into_iter()
                .filter(|m| !m.is_empty())
                .map(Shape::mesh),
        );
        if self.lines.len() > LINE_CACHE_CAPACITY {
            let frame = self.frame;
            self.lines.retain(|_, l| l.last_used == frame);
        }
    }

    pub fn atlas_stats(&self) -> AtlasStats {
        self.atlas.stats()
    }

    pub fn cached_lines(&self) -> usize {
        self.lines.len()
    }
}

fn layout<'a>(
    line: &'a mut BufferLine,
    font_system: &mut FontSystem,
    config: TextConfig,
    pixels_per_point: f32,
    wrap: bool,
    match_mono_width: Option<f32>,
) -> &'a [cosmic_text::LayoutLine] {
    let width = config
        .wrap_width
        .filter(|_| wrap)
        .map(|w| w * pixels_per_point);
    line.layout(
        font_system,
        config.font_size * pixels_per_point,
        width,
        if width.is_some() {
            Wrap::WordOrGlyph
        } else {
            Wrap::None
        },
        Ellipsize::None,
        match_mono_width,
        TAB_WIDTH,
        Hinting::Enabled,
    )
}
