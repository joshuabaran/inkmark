use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use cosmic_text::{
    Attrs, AttrsList, BufferLine, Ellipsize, Family, FontSystem, Hinting, LineEnding, Metrics,
    Shaping, Style, Weight, Wrap,
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

/// Font choices that change layout (colors don't; they apply at paint time).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontStyle {
    pub mono: bool,
    pub bold: bool,
    pub italic: bool,
    /// Size relative to the pane's font size, in percent.
    pub scale: u16,
}

impl Default for FontStyle {
    fn default() -> Self {
        Self {
            mono: false,
            bold: false,
            italic: false,
            scale: 100,
        }
    }
}

/// A line with styled runs. Bytes outside `runs` use the pane's default font.
#[derive(Clone, Copy, Debug)]
pub struct RichLine<'a> {
    pub text: &'a str,
    pub runs: &'a [(Range<usize>, FontStyle)],
    /// Wrap width in points; `None` uses the pane's.
    pub wrap_width: Option<f32>,
}

impl<'a> RichLine<'a> {
    pub fn plain(text: &'a str) -> Self {
        Self {
            text,
            runs: &[],
            wrap_width: None,
        }
    }

    fn key_hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        self.text.hash(&mut h);
        self.runs.hash(&mut h);
        self.wrap_width.map(f32::to_bits).hash(&mut h);
        h.finish()
    }

    fn matches(&self, key: &LineKey) -> bool {
        key.text == self.text
            && key.runs == self.runs
            && key.wrap_width == self.wrap_width.map(f32::to_bits)
    }
}

struct LineKey {
    text: String,
    runs: Vec<(Range<usize>, FontStyle)>,
    wrap_width: Option<u32>,
}

struct CachedLine {
    key: LineKey,
    line: BufferLine,
    last_used: u64,
}

/// The font database and glyph atlas, shared by every pane's renderer.
pub struct Fonts {
    ctx: egui::Context,
    font_system: FontSystem,
    atlas: GlyphAtlas,
    /// egui pass the atlas was last prepared for.
    pass: Option<u64>,
    /// Bumped when the font families change, so renderers re-shape.
    generation: u64,
}

pub type SharedFonts = Rc<RefCell<Fonts>>;

impl Fonts {
    pub fn shared(ctx: &egui::Context) -> SharedFonts {
        Rc::new(RefCell::new(Self {
            ctx: ctx.clone(),
            font_system: FontSystem::new(),
            atlas: GlyphAtlas::new(ctx),
            pass: None,
            generation: 0,
        }))
    }

    /// Whether a font family with this name is installed (any case).
    pub fn has_family(&self, name: &str) -> bool {
        self.installed_family(name).is_some()
    }

    /// The installed family `name` refers to, spelled as the font names
    /// itself: font lookups compare names exactly, so a name typed in
    /// another case must be stored in this spelling. Exact matches win.
    pub fn installed_family(&self, name: &str) -> Option<String> {
        let mut folded = None;
        for face in self.font_system.db().faces() {
            for (family, _) in &face.families {
                if family == name {
                    return Some(family.clone());
                }
                if folded.is_none() && family.eq_ignore_ascii_case(name) {
                    folded = Some(family.clone());
                }
            }
        }
        folded
    }

    /// The families code (monospace) and plain text (sans-serif) use now.
    pub fn families(&self) -> (String, String) {
        use cosmic_text::fontdb::Family as F;
        let db = self.font_system.db();
        (
            db.family_name(&F::Monospace).to_owned(),
            db.family_name(&F::SansSerif).to_owned(),
        )
    }

    /// The families plain text and code use (`None` leaves one as it is).
    /// Names that aren't installed are skipped and returned, so the caller
    /// can say so.
    pub fn set_families(&mut self, monospace: Option<&str>, sans: Option<&str>) -> Vec<String> {
        let mut missing = Vec::new();
        let mut changed = false;
        for (name, mono) in [(monospace, true), (sans, false)] {
            let Some(name) = name else { continue };
            let Some(name) = self.installed_family(name) else {
                missing.push(name.to_owned());
                continue;
            };
            let db = self.font_system.db_mut();
            if mono {
                db.set_monospace_family(name);
            } else {
                db.set_sans_serif_family(name);
            }
            changed = true;
        }
        if changed {
            self.generation += 1;
        }
        missing
    }

    /// Prepares the atlas once per egui pass, however many panes draw.
    fn begin_pass(&mut self) {
        let pass = self.ctx.cumulative_pass_nr();
        if self.pass != Some(pass) {
            self.pass = Some(pass);
            self.atlas.begin_frame();
        }
    }
}

/// Lays out lines with cosmic-text and paints them through the glyph atlas.
///
/// Everything is laid out in physical pixels with metrics hinting, then
/// converted to points at paint time, so glyphs land on the pixel grid at any
/// scale factor. Layouts are cached by content (text, runs, wrap width), so
/// inserting or deleting lines doesn't invalidate the lines around them.
pub struct TextRenderer {
    fonts: SharedFonts,
    config: Option<TextConfig>,
    pixels_per_point: f32,
    /// Line height rounded to whole physical pixels.
    line_height_px: f32,
    /// Average advance of typical text, for height estimates.
    avg_advance_px: f32,
    lines: HashMap<u64, CachedLine>,
    frame: u64,
    /// `Fonts::generation` the cached lines were shaped with.
    font_generation: u64,
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
    /// A renderer with its own fonts; prefer [`with_fonts`](Self::with_fonts)
    /// when several panes are on screen.
    pub fn new(ctx: &egui::Context) -> Self {
        Self::with_fonts(Fonts::shared(ctx))
    }

    pub fn with_fonts(fonts: SharedFonts) -> Self {
        Self {
            fonts,
            config: None,
            pixels_per_point: 1.0,
            line_height_px: 0.0,
            avg_advance_px: 0.0,
            lines: HashMap::new(),
            frame: 0,
            font_generation: 0,
        }
    }

    /// Starts a frame. Returns `true` if layout was invalidated, in which case
    /// callers must re-estimate their line heights.
    pub fn begin_frame(&mut self, config: TextConfig, pixels_per_point: f32) -> bool {
        self.frame += 1;
        let generation = {
            let mut fonts = self.fonts.borrow_mut();
            fonts.begin_pass();
            fonts.generation
        };
        if self.config == Some(config)
            && self.pixels_per_point == pixels_per_point
            && self.font_generation == generation
        {
            return false;
        }
        self.font_generation = generation;
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

    fn attrs(&self, style: FontStyle) -> Attrs<'static> {
        let config = self.config();
        let family = if style.mono || (config.monospace && style == FontStyle::default()) {
            Family::Monospace
        } else {
            Family::SansSerif
        };
        let scale = f32::from(style.scale) / 100.0;
        let ppp = self.pixels_per_point;
        Attrs::new()
            .family(family)
            .weight(if style.bold {
                Weight::BOLD
            } else {
                Weight::NORMAL
            })
            .style(if style.italic {
                Style::Italic
            } else {
                Style::Normal
            })
            .metrics(Metrics::new(
                config.font_size * scale * ppp,
                (config.line_height * scale * ppp).round(),
            ))
    }

    /// A line to shape. `wide` scales the characters starting at those
    /// byte offsets (see `wide_scales`).
    fn new_line(&self, line: RichLine, wide: &HashMap<usize, f32>) -> BufferLine {
        let mut attrs = AttrsList::new(&self.attrs(FontStyle::default()));
        for (range, style) in line.runs {
            attrs.add_span(range.clone(), &self.attrs(*style));
        }
        let config = self.config();
        let em = config.font_size * self.pixels_per_point;
        let line_height = (config.line_height * self.pixels_per_point).round();
        for (&at, &scale) in wide {
            let len = line.text[at..].chars().next().map_or(0, char::len_utf8);
            let attrs_at = self
                .attrs(FontStyle::default())
                .metrics(Metrics::new(em * scale, line_height));
            attrs.add_span(at..at + len, &attrs_at);
        }
        BufferLine::new(line.text, LineEnding::None, attrs, Shaping::Advanced)
    }

    /// Monospace text: how much to scale each wide character (CJK, most
    /// emoji) so it's shaped exactly two cells wide, and wrapping sees the
    /// width it's drawn at. Measured, not assumed: a CJK font draws them 1
    /// em wide, but emoji and the boxes of a missing font differ.
    fn wide_scales(&mut self, line: RichLine) -> HashMap<usize, f32> {
        let config = self.config();
        let cell = self.avg_advance_px;
        let wide: Vec<usize> = line
            .text
            .char_indices()
            .filter(|&(_, c)| unicode_width::UnicodeWidthChar::width(c) == Some(2))
            .map(|(i, _)| i)
            .collect();
        if !config.monospace || cell <= 0.0 || wide.is_empty() {
            return HashMap::new();
        }
        let mut probe = self.new_line(line, &HashMap::new());
        let ppp = self.pixels_per_point;
        let laid = layout(
            &mut probe,
            &mut self.fonts.borrow_mut().font_system,
            config,
            None,
            ppp,
            false,
            None,
        );
        let mut advance: HashMap<usize, f32> = HashMap::new();
        for g in laid.iter().flat_map(|l| l.glyphs.iter()) {
            let a = advance.entry(g.start).or_insert(0.0);
            *a = a.max(g.w);
        }
        wide.into_iter()
            .filter_map(|at| {
                let w = advance.get(&at).copied().filter(|&w| w > 0.0)?;
                Some((at, 2.0 * cell / w))
            })
            .collect()
    }

    /// Monospace text on its cell grid: each glyph's `(x, w)` in pixels
    /// from the row's start, one cell per column its cluster takes (two for
    /// wide characters, a tab to the next stop), and how far to shift the
    /// drawn glyph to center it in its cells. `None` for proportional text.
    fn cells(
        &self,
        glyphs: &[cosmic_text::LayoutGlyph],
        text: &str,
    ) -> Option<Vec<(f32, f32, f32)>> {
        if !self.config().monospace || self.avg_advance_px <= 0.0 {
            return None;
        }
        let cell = self.avg_advance_px;
        let mut col = 0usize;
        let mut out: Vec<(f32, f32, f32)> = Vec::with_capacity(glyphs.len());
        let mut prev: Option<(usize, (f32, f32, f32))> = None;
        for g in glyphs {
            // More glyphs of one cluster (combining marks): the same cells.
            if let Some((start, placed)) = prev
                && start == g.start
            {
                out.push(placed);
                continue;
            }
            let cluster = text.get(g.start..g.end).unwrap_or("");
            let cols = if cluster == "\t" {
                TAB_WIDTH as usize - col % TAB_WIDTH as usize
            } else {
                unicode_width::UnicodeWidthStr::width(cluster).max(1)
            };
            let (x, w) = (col as f32 * cell, cols as f32 * cell);
            let center = if cluster == "\t" {
                0.0
            } else {
                ((w - g.w) / 2.0).max(0.0)
            };
            let placed = (x, w, center);
            out.push(placed);
            prev = Some((g.start, placed));
            col += cols;
        }
        Some(out)
    }

    /// Average advance of typical text, in points. Zero before the first
    /// [`begin_frame`](Self::begin_frame), and when the scale is not positive.
    pub fn avg_advance(&self) -> f32 {
        let scale = self.pixels_per_point;
        if scale <= 0.0 {
            0.0
        } else {
            self.avg_advance_px / scale
        }
    }

    fn measure_avg_advance(&mut self) -> f32 {
        const SAMPLE: &str = "The quick brown fox jumps over the lazy dog, then naps; 0123456789.";
        let (config, ppp) = (self.config(), self.pixels_per_point);
        let mut line = self.new_line(RichLine::plain(SAMPLE), &HashMap::new());
        let width = layout(
            &mut line,
            &mut self.fonts.borrow_mut().font_system,
            config,
            None,
            ppp,
            false,
            None,
        )
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

    /// Scale from the last [`begin_frame`](Self::begin_frame). One before that.
    pub fn pixels_per_point(&self) -> f32 {
        self.pixels_per_point
    }

    /// Lays out `text` (cached) and returns its height in points.
    pub fn line_height(&mut self, text: &str) -> f32 {
        self.rich_height(RichLine::plain(text))
    }

    pub fn rich_height(&mut self, line: RichLine) -> f32 {
        self.rich_geometry(line).height()
    }

    /// Caret and hit-test geometry of `text`, in points from its top-left.
    pub fn geometry(&mut self, text: &str) -> LineGeometry {
        self.rich_geometry(RichLine::plain(text))
    }

    pub fn rich_geometry(&mut self, line: RichLine) -> LineGeometry {
        let ppp = self.pixels_per_point;
        let line_height_px = self.line_height_px;
        let row_height = self.row_height();
        let text = line.text;
        let text_len = text.len();
        self.cached_line(line);
        let hash = line.key_hash();
        let buffer_line = &self.lines[&hash].line;
        let mut rows: Vec<Row> = Vec::new();
        for run in buffer_line.layout_runs(None, line_height_px) {
            let cells = self.cells(run.glyphs, text);
            let clusters: Vec<ClusterSpan> = run
                .glyphs
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    let (x, w) = cells.as_ref().map_or((g.x, g.w), |c| (c[i].0, c[i].1));
                    ClusterSpan {
                        start: g.start,
                        end: g.end,
                        x: x / ppp,
                        w: w / ppp,
                    }
                })
                .collect();
            let prev_end = rows.last().map_or(0, |r| r.end);
            let start = clusters.iter().map(|c| c.start).min().unwrap_or(prev_end);
            let end = clusters.iter().map(|c| c.end).max().unwrap_or(prev_end);
            let ends_in_space = clusters.last().is_some_and(|c| {
                text.get(c.start..c.end)
                    .is_some_and(|t| !t.is_empty() && t.chars().all(char::is_whitespace))
            });
            rows.push(Row {
                top: run.line_top / ppp,
                height: run.line_height / ppp,
                start,
                end,
                clusters,
                ends_in_space,
            });
        }
        if rows.is_empty() {
            rows.push(Row {
                height: row_height,
                ..Row::default()
            });
        }
        // The last row owns everything to the end of the line.
        if let Some(last) = rows.last_mut() {
            last.end = last.end.max(text_len);
        }
        LineGeometry { rows }
    }

    fn cached_line(&mut self, line: RichLine) -> &mut BufferLine {
        let (frame, config, ppp) = (self.frame, self.config(), self.pixels_per_point);
        // Snap glyphs from monospace fallback fonts (e.g. CJK) to whole cells.
        let mono_width = config.monospace.then_some(self.avg_advance_px);
        let hash = line.key_hash();
        let hit = self.lines.get(&hash).is_some_and(|c| line.matches(&c.key));
        if !hit {
            let scales = self.wide_scales(line);
            let mut buffer_line = self.new_line(line, &scales);
            layout(
                &mut buffer_line,
                &mut self.fonts.borrow_mut().font_system,
                config,
                line.wrap_width,
                ppp,
                true,
                mono_width,
            );
            self.lines.insert(
                hash,
                CachedLine {
                    key: LineKey {
                        text: line.text.to_owned(),
                        runs: line.runs.to_vec(),
                        wrap_width: line.wrap_width.map(f32::to_bits),
                    },
                    line: buffer_line,
                    last_used: frame,
                },
            );
        }
        let cached = self.lines.get_mut(&hash).expect("inserted above");
        cached.last_used = frame;
        &mut cached.line
    }

    /// Paints `text` with its top-left corner at `top_left` (points).
    pub fn draw_line(&mut self, out: &mut GlyphMeshes, text: &str, top_left: Pos2, color: Color32) {
        self.draw_rich(out, RichLine::plain(text), top_left, color, &[], &[]);
    }

    /// Like [`draw_line`](Self::draw_line), with byte ranges of `text` drawn
    /// in other colors.
    pub fn draw_line_colored(
        &mut self,
        out: &mut GlyphMeshes,
        text: &str,
        top_left: Pos2,
        color: Color32,
        colors: &[(Range<usize>, Color32)],
    ) {
        self.draw_rich(out, RichLine::plain(text), top_left, color, colors, &[]);
    }

    /// Paints a rich line. `colors` must be sorted and non-overlapping; colors
    /// apply at paint time, so recoloring never re-shapes. Glyphs whose start
    /// lies in `skip` are omitted, so a stand-in gap can be drawn over.
    pub fn draw_rich(
        &mut self,
        out: &mut GlyphMeshes,
        line: RichLine,
        top_left: Pos2,
        color: Color32,
        colors: &[(Range<usize>, Color32)],
        skip: &[Range<usize>],
    ) {
        let ppp = self.pixels_per_point;
        let line_height_px = self.line_height_px;
        let origin = ((top_left.x * ppp).round(), (top_left.y * ppp).round());
        // Lay out first: that may borrow the fonts itself.
        let hash = line.key_hash();
        self.cached_line(line);
        let mut fonts = self.fonts.borrow_mut();
        let Fonts {
            font_system, atlas, ..
        } = &mut *fonts;
        let text = &self.lines[&hash].key.text;
        let buffer_line = &self.lines[&hash].line;
        for run in buffer_line.layout_runs(None, line_height_px) {
            let cells = self.cells(run.glyphs, text);
            for (i, glyph) in run.glyphs.iter().enumerate() {
                if skip
                    .iter()
                    .any(|r| r.start <= glyph.start && glyph.start < r.end)
                {
                    continue;
                }
                let mut placed = glyph.clone();
                if let Some(cells) = &cells {
                    placed.x = cells[i].0 + cells[i].2;
                }
                let physical = placed.physical((origin.0, origin.1 + run.line_y), 1.0);
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
        self.fonts.borrow_mut().atlas.upload();
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
        self.fonts.borrow().atlas.stats()
    }

    pub fn cached_lines(&self) -> usize {
        self.lines.len()
    }
}

fn layout<'a>(
    line: &'a mut BufferLine,
    font_system: &mut FontSystem,
    config: TextConfig,
    wrap_width: Option<f32>,
    pixels_per_point: f32,
    wrap: bool,
    match_mono_width: Option<f32>,
) -> &'a [cosmic_text::LayoutLine] {
    let width = wrap_width
        .or(config.wrap_width)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TextConfig {
        TextConfig {
            monospace: false,
            font_size: 16.0,
            line_height: 26.0,
            wrap_width: Some(600.0),
        }
    }

    #[test]
    fn avg_advance_stays_in_points_below_scale_one() {
        let ctx = egui::Context::default();
        let mut text = TextRenderer::new(&ctx);
        let config = config();
        assert_eq!(text.avg_advance(), 0.0);
        text.begin_frame(config, 1.0);
        let at_one = text.avg_advance();
        text.begin_frame(config, 0.5);
        let at_half = text.avg_advance();
        text.begin_frame(config, 2.0);
        let at_two = text.avg_advance();
        assert!(at_one > 1.0, "{at_one}");
        assert!(
            (at_half - at_one).abs() / at_one < 0.2,
            "scale 0.5 is {at_half}, scale 1 is {at_one}"
        );
        assert!(
            (at_two - at_one).abs() / at_one < 0.2,
            "scale 2 is {at_two}, scale 1 is {at_one}"
        );
        text.begin_frame(config, 0.0);
        let at_zero = text.avg_advance();
        assert!(at_zero.is_finite() && at_zero >= 0.0, "{at_zero}");
    }
}
