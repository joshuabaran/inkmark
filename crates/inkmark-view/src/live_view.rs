//! The rendered pane: leaf blocks laid out as rich text, Markdown syntax
//! hidden except on the caret's line. Navigable but read-only for now; edits
//! arrive with source-patching in M4.

use std::ops::Range;

use egui::output::IMEOutput;
use egui::{
    Color32, CursorIcon, Event, IMEPurpose, Id, ImeEvent, Key, Modifiers, Pos2, Rect, Response,
    Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use inkmark_buffer::{Bias, Document, Edit, EditKind, Selection};
use inkmark_parse::{
    BlockKind, Leaf, ParseOutput, ParseState, SourceMap, SpanKind, Style, Syntax, inline_images,
};
use inkmark_text::{
    GlyphMeshes, LineGeometry, RichLine, ScrollAnchor, SharedFonts, TextConfig, TextRenderer,
};

use crate::commands::{self, EditPlan, EnterContext};
use crate::images::{ImageCache, ImageSlot};
use crate::keys::{self, Action};
use crate::lines::{self, LineIndex, SCROLLBAR_WIDTH, ScrollPos, Synced};
use crate::live_layout::{self, LeafLayout, LeafStyle, Reveal};
use crate::motion;
use crate::tables;
use crate::theme::{self, Theme};

const PADDING: f32 = 28.0;
const QUOTE_INDENT: f32 = 22.0;
const ITEM_INDENT: f32 = 28.0;
/// Room for a footnote definition's `[label]` in the margin.
const FOOTNOTE_INDENT: f32 = 44.0;
const CODE_PAD: f32 = 10.0;
const CARET_WIDTH: f32 = 2.0;
const NEWLINE_WIDTH: f32 = 6.0;
/// Space between a paragraph's text and an image below it.
const IMAGE_GAP: f32 = 6.0;
/// Table cell padding and the space above and below a table.
const CELL_PAD_X: f32 = 10.0;
const CELL_PAD_Y: f32 = 5.0;
const TABLE_PAD: f32 = 6.0;
/// Narrowest a table column gets before its text wraps.
const MIN_COLUMN: f32 = 48.0;
const REVEAL_FRAMES: u8 = 3;
/// Height of a blank source line, in rows.
const BLANK_LINE: f32 = 0.6;

/// Laid-out text: a leaf's (or a table cell's) display segments.
struct Body {
    layout: LeafLayout,
    seg_tops: Vec<f32>,
    geometry: Vec<LineGeometry>,
    /// The source bytes it shows.
    range: Range<usize>,
    /// Width it was wrapped at: drawing must use the same layout.
    wrap: f32,
}

impl Body {
    /// The segment at height `y` (from the body's top).
    fn segment_at(&self, y: f32) -> usize {
        self.seg_tops.iter().rposition(|&t| t <= y).unwrap_or(0)
    }

    /// Caret rect for source offset `at`, from the body's top-left.
    /// `upstream`: at a mid-word wrap, at the end of the row before.
    fn caret_rect(&self, at: usize, upstream: bool) -> Rect {
        let (seg, d) = self.layout.display_pos(at);
        self.geometry[seg]
            .caret_rect_affine(d, CARET_WIDTH, upstream)
            .translate(vec2(0.0, self.seg_tops[seg]))
    }

    /// Source offset at `local` (from the body's top-left), and whether it
    /// is the end of a mid-word-wrapped row (upstream).
    fn hit(&self, local: Vec2) -> (usize, bool) {
        let seg = self.segment_at(local.y);
        let at = vec2(local.x, local.y - self.seg_tops[seg]);
        let (d, upstream) = self.geometry[seg].hit_affine(at);
        if let Some(src) =
            crate::math::formula_hit(&self.layout.segments[seg], &self.geometry[seg], at.x, d)
        {
            return (src, false);
        }
        (self.layout.source_pos(seg, d), upstream)
    }

    /// Start or end of the visual row holding `at` (on the row before at a
    /// wrap point when `upstream`), and whether that end is upstream.
    fn row_edge(&self, at: usize, end: bool, upstream: bool) -> (usize, bool) {
        let (seg, d) = self.layout.display_pos(at);
        let g = &self.geometry[seg];
        let row = g.row_of_affine(d, upstream);
        let (d, upstream) = if end {
            g.hit_row_affine(row, f32::INFINITY)
        } else {
            (g.rows[row].start, false)
        };
        (self.layout.source_pos(seg, d), upstream)
    }

    /// Widest visual row, for table column sizing and alignment.
    fn width(&self) -> f32 {
        self.geometry
            .iter()
            .flat_map(|g| g.rows.iter())
            .filter_map(|r| r.clusters.last().map(|c| c.x + c.w))
            .fold(0.0, f32::max)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// A GFM table laid out as a grid. Coordinates are from the table's
/// top-left (the leaf's text origin).
struct Grid {
    cells: Vec<GridCell>,
    col_x: Vec<f32>,
    col_w: Vec<f32>,
    row_y: Vec<f32>,
    row_h: Vec<f32>,
    has_head: bool,
}

struct GridCell {
    body: Body,
    /// Where the cell's text starts.
    origin: Vec2,
}

impl Grid {
    fn width(&self) -> f32 {
        self.col_x
            .last()
            .zip(self.col_w.last())
            .map_or(0.0, |(x, w)| x + w)
    }

    fn height(&self) -> f32 {
        self.row_y
            .last()
            .zip(self.row_h.last())
            .map_or(0.0, |(y, h)| y + h)
    }

    /// The cell showing source offset `at` (or the nearest one after it).
    fn cell_for(&self, at: usize) -> Option<&GridCell> {
        self.cells
            .iter()
            .find(|c| c.body.range.start <= at && at <= c.body.range.end)
            .or_else(|| self.cells.iter().find(|c| c.body.range.start >= at))
            .or_else(|| self.cells.last())
    }

    /// The cell under `local`: by row, then by column.
    fn cell_at(&self, local: Vec2) -> Option<&GridCell> {
        let row = self.row_y.iter().rposition(|&y| y <= local.y).unwrap_or(0);
        let col = self.col_x.iter().rposition(|&x| x <= local.x).unwrap_or(0);
        let top = self.row_y.get(row).copied().unwrap_or(0.0);
        let left = self.col_x.get(col).copied().unwrap_or(0.0);
        self.cells
            .iter()
            .filter(|c| (c.origin.y - top).abs() < self.row_h[row] && c.origin.y >= top)
            .min_by(|a, b| {
                (a.origin.x - left)
                    .abs()
                    .total_cmp(&(b.origin.x - left).abs())
            })
            .filter(|_| !self.cells.is_empty())
            .or_else(|| self.cells.last())
    }
}

/// A leaf block laid out for this frame.
struct Placed {
    leaf: Leaf,
    body: Body,
    /// Set for tables: the grid replaces `body` for drawing and hit-testing.
    table: Option<Grid>,
    first_line: usize,
    last_line: usize,
    /// Text left edge, from the pane's content left.
    indent: f32,
    /// An image-only paragraph away from the caret shows just its images.
    text_hidden: bool,
    images: Vec<PlacedImage>,
    height: f32,
}

impl Placed {
    /// Caret rect for `at`, from the leaf's text origin.
    fn caret_rect(&self, at: usize, upstream: bool) -> Rect {
        match self.table.as_ref().and_then(|g| g.cell_for(at)) {
            Some(cell) => cell
                .body
                .caret_rect(at, upstream)
                .translate(cell.origin + vec2(0.0, TABLE_PAD)),
            None => self.body.caret_rect(at, upstream),
        }
    }

    /// Source offset at `local`, from the leaf's text origin, and whether
    /// it's upstream.
    fn hit(&self, local: Vec2) -> (usize, bool) {
        let local_in_table = local - vec2(0.0, TABLE_PAD);
        match self.table.as_ref().and_then(|g| g.cell_at(local_in_table)) {
            Some(cell) => cell.body.hit(local_in_table - cell.origin),
            None => self.body.hit(local),
        }
    }

    fn row_edge(&self, at: usize, end: bool, upstream: bool) -> (usize, bool) {
        match self.table.as_ref().and_then(|g| g.cell_for(at)) {
            Some(cell) => cell.body.row_edge(at, end, upstream),
            None => self.body.row_edge(at, end, upstream),
        }
    }
}

/// An image below a leaf's text, in leaf coordinates.
struct PlacedImage {
    top: f32,
    size: Vec2,
    slot: ImageSlot,
    dest: String,
}

#[derive(Clone, Copy)]
struct Frame {
    rect: Rect,
    /// Content left edge and width (inside padding, left of the scrollbar).
    left: f32,
    width: f32,
}

pub struct LiveView {
    id: Id,
    text: TextRenderer,
    lines: LineIndex,
    selection: Selection,
    preferred_x: Option<f32>,
    reveal_caret: u8,
    scrolled: bool,
    focused: bool,
    dragging: bool,
    /// IME composition shown at the caret until committed.
    preedit: String,
    /// Per-pane minimap toggle; survives mode switches with the view.
    pub show_minimap: bool,
    /// The caret moved since the last edit: start a new undo step.
    seal_undo: bool,
    /// A short explanation for a key that did nothing, for the status bar.
    hint: Option<&'static str>,
    /// A link was Ctrl+clicked at this offset; the app follows it.
    follow: Option<usize>,
    /// The table the caret is in: its start, and its text when the caret
    /// came in. Leaving it changed re-pads it.
    table_visit: Option<(usize, String)>,
    /// Colors, refreshed from the context every frame.
    theme: std::sync::Arc<Theme>,
    /// The caret at this offset is at the end of a row that wrapped
    /// mid-word (End, or a click past the row), not the start of the next
    /// row. Stale as soon as the caret is anywhere else.
    upstream_at: Option<usize>,
    /// Whether the last `offset_at` hit was such a row end.
    hit_upstream: bool,
    /// The selection was set from outside in current offsets: don't map it
    /// through edits on the next sync.
    selection_current: bool,
    /// A scroll position set from outside, applied after the next sync.
    pending_scroll: Option<ScrollPos>,
    /// Lines whose block an edit changed while it was measured. The next
    /// settle lays each of those blocks out again, wherever it is, so a
    /// block never keeps a height from before the edit. Blocks the view
    /// hasn't reached stay estimated; nothing walks from the top.
    remeasure: Vec<usize>,
    /// Task checkboxes drawn last frame: hit area, source offset of the
    /// `[ ]` marker, checked.
    checkboxes: Vec<(Rect, usize, bool)>,
    /// Created on the first frame, when an egui context is at hand.
    images: Option<ImageCache>,
    /// Formula drawings for the live pane. The document bytes are not rewritten.
    math: crate::math::MathCache,
    /// Context of the frame being drawn, so a formula texture can be uploaded.
    ctx: Option<egui::Context>,
    pub font_size: f32,
    pub line_height: f32,
    /// Shortcuts, shared with the code pane and the app shell.
    keys: keys::KeyMap,
}

impl LiveView {
    pub fn new(ctx: &egui::Context, id: Id) -> Self {
        Self::with_fonts(inkmark_text::Fonts::shared(ctx), id)
    }

    pub fn with_fonts(fonts: SharedFonts, id: Id) -> Self {
        Self {
            id,
            text: TextRenderer::with_fonts(fonts),
            lines: LineIndex::new(),
            selection: Selection::default(),
            preferred_x: None,
            reveal_caret: 0,
            scrolled: false,
            focused: false,
            dragging: false,
            preedit: String::new(),
            show_minimap: true,
            seal_undo: false,
            hint: None,
            follow: None,
            table_visit: None,
            theme: std::sync::Arc::new(Theme::dark()),
            upstream_at: None,
            hit_upstream: false,
            selection_current: false,
            pending_scroll: None,
            remeasure: Vec::new(),
            checkboxes: Vec::new(),
            images: None,
            math: crate::math::MathCache::default(),
            ctx: None,
            font_size: 16.0,
            line_height: 26.0,
            keys: keys::KeyMap::builtin(),
        }
    }

    /// Replaces the shortcuts. The app does this when config.toml changes.
    pub fn set_keys(&mut self, keys: keys::KeyMap) {
        self.keys = keys;
    }

    /// The offset of a link Ctrl+clicked since the last call.
    pub fn take_follow(&mut self) -> Option<usize> {
        self.follow.take()
    }

    /// Why the last key did nothing, if it's not obvious. Taken once.
    pub fn take_hint(&mut self) -> Option<&'static str> {
        self.hint.take()
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// The height currently stored for `line`.
    pub fn measured_height(&self, line: usize) -> f32 {
        if line < self.lines.heights.len() {
            self.lines.heights.height(line)
        } else {
            0.0
        }
    }

    /// Whether `line` has a laid-out height rather than an estimate.
    pub fn line_measured(&self, line: usize) -> bool {
        line < self.lines.heights.len() && self.lines.heights.is_measured(line)
    }

    /// The top of the view: a source line and how far into its stored
    /// height the view starts, in points.
    pub fn view_top(&self) -> (usize, f32) {
        (self.lines.anchor.line, self.lines.anchor.offset)
    }

    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
        self.selection_current = true;
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
    }

    /// Shows `selection` without scrolling to it (mirroring the other pane).
    pub fn mirror_selection(&mut self, selection: Selection) {
        self.selection = selection;
        self.selection_current = true;
    }

    pub fn reset(&mut self) {
        self.lines.reset();
        self.remeasure.clear();
        self.selection = Selection::default();
        self.preferred_x = None;
    }

    /// Gives up keyboard focus (e.g. while a dialog is open).
    pub fn release_focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.surrender_focus(self.id));
    }

    pub fn request_focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.request_focus(self.id));
    }

    pub fn has_focus(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| m.has_focus(self.id))
    }

    /// Total height of the document as laid out so far (unmeasured lines
    /// are estimates), in points.
    pub fn content_height(&self) -> f64 {
        self.lines.heights.total()
    }

    pub fn take_scrolled(&mut self) -> bool {
        std::mem::take(&mut self.scrolled)
    }

    /// Where the view is scrolled to, in source lines: a position inside a
    /// block counts proportionally through the block's lines.
    pub fn scroll_pos(&self, doc: &Document, parse: &ParseOutput) -> ScrollPos {
        if let Some(pos) = self.pending_scroll {
            return pos;
        }
        let heights = &self.lines.heights;
        let anchor = self.lines.anchor;
        if anchor.line >= heights.len() {
            return ScrollPos::default();
        }
        match leaf_lines(doc, parse, anchor.line) {
            Some((first, last)) => {
                // The block's height is everything from its first line to
                // the line after it: measured, it all sits on the first
                // line; not yet drawn, it's spread over every line's estimate.
                let top = heights.offset_of(first);
                let height = (heights.offset_of(last + 1) - top).max(1.0);
                let progress = ((heights.anchor_y(anchor) - top) / height).clamp(0.0, 1.0) as f32;
                let lines = progress * (last - first + 1) as f32;
                ScrollPos {
                    line: (first + lines as usize).min(last),
                    frac: lines.fract(),
                }
            }
            _ => ScrollPos {
                line: anchor.line,
                frac: anchor.offset / heights.height(anchor.line).max(1.0),
            },
        }
    }

    /// Scrolls to `pos`, the inverse of [`scroll_pos`](Self::scroll_pos).
    /// The position is current for the document as it is now; it's applied
    /// once the view has caught up with the document (so a pane that was
    /// hidden doesn't shift it through the same edits again).
    pub fn set_scroll_pos(&mut self, _doc: &Document, _parse: &ParseOutput, pos: ScrollPos) {
        self.pending_scroll = Some(pos);
    }

    fn apply_scroll_pos(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        pos: ScrollPos,
        width: f32,
    ) {
        if pos.line >= self.lines.heights.len() {
            return;
        }
        let Some(leaf) = leaf_at_line(doc, parse, pos.line) else {
            let heights = &self.lines.heights;
            let y = heights.offset_of(pos.line) + f64::from(pos.frac * heights.height(pos.line));
            self.lines.anchor = heights.line_at(y);
            return;
        };
        // Lay the block out first, so the position inside it uses its real
        // height rather than estimates that drawing would then collapse.
        let p = self.place(doc, parse, leaf, width);
        self.record_block(p.first_line, p.first_line, p.last_line, p.height);
        let progress =
            ((pos.line - p.first_line) as f32 + pos.frac) / (p.last_line - p.first_line + 1) as f32;
        self.lines.anchor = ScrollAnchor {
            line: p.first_line,
            offset: progress.clamp(0.0, 1.0) * p.height,
        };
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        doc: &mut Document,
        parse: Option<&mut ParseState>,
    ) -> Response {
        self.theme = theme::current(ui.ctx());
        let rect = ui.available_rect_before_wrap();
        ui.advance_cursor_after_rect(rect);
        let (bar, text_right, minimap) = self.pane_bounds(rect);
        let available = (text_right - rect.left() - 2.0 * PADDING).max(80.0);
        let frame = Frame {
            rect,
            left: rect.left() + PADDING,
            width: available,
        };
        let text_rect = Rect::from_min_max(rect.min, pos2(text_right, rect.bottom()));
        let response = ui.interact(text_rect, self.id, Sense::click_and_drag());
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Text);
        }
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        lines::lock_focus(ui, self.id);
        self.focused = response.has_focus();

        self.ctx = Some(ui.ctx().clone());
        self.math.begin_frame();
        let ppp = ui.ctx().pixels_per_point();
        let config_for = |width: f32| TextConfig {
            monospace: false,
            font_size: self.font_size,
            line_height: self.line_height,
            wrap_width: Some(width),
        };
        // The live pane wraps at its content column.
        if self.text.begin_frame(config_for(available), ppp) {
            self.lines.invalidate();
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, self.theme.background);
        if self.images.is_none() {
            self.images = Some(ImageCache::new(ui.ctx()));
        }
        let Some(state) = parse else {
            return response;
        };
        if let Some(wait) = state.update(doc) {
            ui.ctx().request_repaint_after(wait);
        }
        if state.output().map.len() != doc.len() {
            return response;
        }
        self.sync(doc, true);
        if let Some(pos) = self.pending_scroll.take() {
            self.apply_scroll_pos(doc, state.output(), pos, frame.width);
        }

        // A click on a task checkbox toggles it (a one-byte source patch)
        // instead of moving the caret.
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let on_box = pointer.and_then(|p| {
            self.checkboxes
                .iter()
                .find(|(r, ..)| r.contains(p))
                .copied()
        });
        if on_box.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let mut box_clicked = false;
        if let Some((_, at, checked)) = on_box
            && ui.input(|i| i.pointer.primary_pressed())
        {
            let edit = Edit::replace(at + 1..at + 2, if checked { " " } else { "x" });
            if doc
                .apply(vec![edit], self.selection, self.selection, EditKind::Other)
                .is_ok()
            {
                self.after_edit(doc);
                self.reveal_caret = 0;
                if let Some(wait) = state.update(doc) {
                    ui.ctx().request_repaint_after(wait);
                }
            }
            box_clicked = true;
        }

        if self.focused {
            self.handle_events(ui, doc, state, frame);
        } else {
            self.preedit.clear();
        }
        let parse = state.output();
        let link = crate::link_under_pointer(ui, &response, Some(parse), |pos| {
            self.offset_at(doc, parse, frame, pos)
        });
        // Over a link with Ctrl held, clicks follow it and nothing else
        // (not even the release half of a double click).
        match link {
            Some((at, pressed)) => {
                if pressed {
                    self.follow = Some(at);
                }
            }
            None if !box_clicked => self.handle_pointer(ui, &response, doc, parse, frame),
            None => {}
        }
        if self.context_menu(&response, doc, state, frame) {
            ui.ctx().request_repaint();
        }
        self.repad_left_table(doc, state);
        let parse = state.output();
        let viewport = rect.height();
        if self.reading_scroll(ui, doc, parse, &response, frame) {
            self.scrolled = true;
        }
        if self.reveal_caret > 0 && self.scroll_caret_into_view(ui, doc, parse, frame) {
            self.scrolled = true;
            self.settle(doc, parse, frame.width, viewport, 0.0);
        }
        let caret = self.paint(&painter, doc, parse, frame);
        self.lines.paint_scrollbar(&painter, bar, &self.theme);
        if let Some(r) = minimap {
            self.paint_minimap(&painter, ui, doc, parse, r, rect.height());
        }
        if self.focused {
            let to_global = ui
                .ctx()
                .layer_transform_to_global(ui.layer_id())
                .unwrap_or_default();
            let cursor = caret.unwrap_or(Rect::from_min_size(rect.min, vec2(1.0, 1.0)));
            ui.output_mut(|o| {
                o.ime = Some(IMEOutput {
                    purpose: IMEPurpose::Normal,
                    rect: to_global * rect,
                    cursor_rect: to_global * cursor,
                    should_interrupt_composition: false,
                })
            });
        }
        response
    }

    /// Brings heights (and, for edits made elsewhere, the selection) up to
    /// date with `doc`.
    fn sync(&mut self, doc: &Document, map_selection: bool) {
        let text = &self.text;
        // A selection set from outside is already in current offsets.
        let map_selection = map_selection && !std::mem::take(&mut self.selection_current);
        match self.lines.sync(doc, |chars| text.estimate_height(chars)) {
            Synced::Rebuilt => self.remeasure.clear(),
            Synced::Changed(changes) => {
                // A rebuild that still had edits to map clears every
                // measurement and reports Changed: nothing to re-measure.
                // A splice leaves the lines around the edit measured, so
                // the blocks at each end of an edit are laid out again.
                if self.lines.heights.measured_count() == 0 {
                    self.remeasure.clear();
                } else {
                    for c in &changes {
                        let l = c.lines;
                        for line in &mut self.remeasure {
                            if *line > l.start + l.removed {
                                *line = *line - l.removed + l.inserted;
                            } else if *line > l.start {
                                *line = l.start;
                            }
                        }
                        self.remeasure.push(l.start);
                        self.remeasure.push(l.start + l.inserted);
                    }
                    self.remeasure.sort_unstable();
                    self.remeasure.dedup();
                }
                if map_selection {
                    for c in &changes {
                        self.selection.anchor = c.map(self.selection.anchor, Bias::Left);
                        self.selection.head = c.map(self.selection.head, Bias::Left);
                    }
                }
            }
            Synced::Unchanged => {}
        }
        self.selection.anchor = self.selection.anchor.min(doc.len());
        self.selection.head = self.selection.head.min(doc.len());
    }

    /// A source line outside any leaf, as the live pane shows it: the line,
    /// or the long-line notice for one too long to shape (before the first
    /// parse every line is outside a leaf, a 1 MB one included).
    fn raw_line_text(doc: &Document, line: usize) -> std::borrow::Cow<'_, str> {
        let range = doc.line_range(line);
        if range.len() > crate::long_line::LONG_LINE {
            crate::long_line::notice(range.len()).into()
        } else {
            doc.slice(range)
        }
    }

    /// Height of one source line that is not part of a leaf, matching paint.
    fn raw_line_height(&mut self, text: &str) -> f32 {
        let blank = text
            .trim_start_matches(|c: char| c.is_whitespace() || c == '>')
            .is_empty();
        if blank {
            self.text.row_height() * BLANK_LINE
        } else {
            self.text.line_height(text)
        }
    }

    /// Records a leaf the way paint does: the whole height on the first
    /// line, and the rest of its lines at zero. A second leaf that starts
    /// on `walked`'s line stacks under the height already stored there.
    fn record_block(&mut self, walked: usize, first: usize, last: usize, height: f32) {
        let len = self.lines.heights.len();
        if first >= len {
            return;
        }
        if first < walked {
            let prev = self.lines.heights.height(first);
            self.lines.heights.set_measured(first, prev + height);
        } else {
            self.lines.heights.set_measured(first, height);
        }
        for l in first + 1..=last {
            if l < len {
                self.lines.heights.set_measured(l, 0.0);
            }
        }
    }

    /// Lays out the block on `line` and stores its height. Returns the
    /// first line after that block.
    fn store_block(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        line: usize,
        width: f32,
    ) -> usize {
        if line >= self.lines.heights.len() {
            return line;
        }
        let Some(leaf) = leaf_at_line(doc, parse, line) else {
            let text = Self::raw_line_text(doc, line);
            let height = self.raw_line_height(&text);
            self.lines.heights.set_measured(line, height);
            return line + 1;
        };
        let placed = self.place(doc, parse, leaf, width);
        // `line` can sit in the middle of the leaf after an edit. The height
        // lives on the first line, so record from there and replace what was
        // stored. Paint stacks a second leaf on that line by walking past it.
        self.record_block(
            placed.first_line,
            placed.first_line,
            placed.last_line,
            placed.height,
        );
        placed.last_line.max(line) + 1
    }

    /// If the anchor sits in a block that is still estimated, remember how
    /// far through that estimate it was, measure the block, and park on the
    /// first line at the same fraction of the real height.
    fn collapse_anchor_block(&mut self, doc: &Document, parse: &ParseOutput, width: f32) {
        let line = self.lines.anchor.line;
        if line >= self.lines.heights.len() {
            return;
        }
        if let Some((first, last)) = leaf_lines(doc, parse, line) {
            if self.lines.heights.is_measured(first) {
                return;
            }
            let top = self.lines.heights.offset_of(first);
            let bottom = self
                .lines
                .heights
                .offset_of((last + 1).min(self.lines.heights.len()));
            let span = (bottom - top).max(1e-3);
            let y = self.lines.heights.anchor_y(self.lines.anchor);
            let progress = ((y - top) / span).clamp(0.0, 1.0);
            self.store_block(doc, parse, first, width);
            let real = self.lines.heights.height(first);
            self.lines.anchor = ScrollAnchor {
                line: first,
                offset: (progress as f32) * real,
            };
            return;
        }
        if self.lines.heights.is_measured(line) {
            return;
        }
        let old = self.lines.heights.height(line).max(0.001);
        let frac = (self.lines.anchor.offset / old).clamp(0.0, 1.0);
        let text = Self::raw_line_text(doc, line);
        let height = self.raw_line_height(&text);
        self.lines.heights.set_measured(line, height);
        self.lines.anchor.offset = frac * height;
    }

    /// The first line of the block that holds `line`: its leaf's first
    /// line, or `line` itself outside any leaf.
    fn block_start(doc: &Document, parse: &ParseOutput, line: usize) -> usize {
        leaf_lines(doc, parse, line).map_or(line, |(first, _)| first)
    }

    /// Whether any line of the block that starts at `start` and runs up to
    /// (not including) `end` still has an estimate, or a height from
    /// before an edit.
    fn block_unmeasured(&self, start: usize, end: usize) -> bool {
        let len = self.lines.heights.len();
        (start..end.max(start + 1).min(len)).any(|l| !self.lines.heights.is_measured(l))
    }

    /// Lays out again the blocks an edit changed since the last settle,
    /// above or below the view. A block in view is laid out again by
    /// `remeasure_visible` anyway, and a block whose lines are all estimates
    /// is left alone: nothing there is stale.
    fn remeasure_edited(&mut self, doc: &Document, parse: &ParseOutput, width: f32, viewport: f32) {
        let len = doc.line_count().min(self.lines.heights.len());
        if len == 0 {
            self.remeasure.clear();
            return;
        }
        let heights = &self.lines.heights;
        let view_top = Self::block_start(doc, parse, self.lines.anchor.line.min(len - 1));
        let view_bottom = heights
            .line_at(heights.anchor_y(self.lines.anchor) + f64::from(viewport))
            .line;
        for line in std::mem::take(&mut self.remeasure) {
            if line >= len {
                continue;
            }
            let (first, last) = leaf_lines(doc, parse, line).unwrap_or((line, line));
            if first <= view_bottom && last >= view_top {
                continue;
            }
            let any_measured =
                (first..=last.min(len - 1)).any(|l| self.lines.heights.is_measured(l));
            if any_measured {
                self.store_block(doc, parse, line, width);
            }
        }
    }

    /// Measures the blocks from line `from` down until they cover `amount`
    /// points below `from`'s top. A block that starts inside that span is
    /// measured whole, including the part that hangs below it. Blocks
    /// already measured are left as they are.
    fn measure_below(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        from: usize,
        amount: f64,
        width: f32,
    ) {
        let count = doc.line_count().min(self.lines.heights.len());
        let mut line = from.min(count);
        while line < count {
            let heights = &self.lines.heights;
            if line > from && heights.offset_of(line) - heights.offset_of(from) >= amount {
                break;
            }
            if heights.is_measured(line) {
                line += 1;
                continue;
            }
            let next = self.store_block(doc, parse, line, width);
            if next <= line {
                break;
            }
            line = next;
        }
    }

    /// Measures the blocks above line `from` until they cover `amount`
    /// points: everything a scroll or a caret moving up by that much passes
    /// through. Nothing further up is touched, so the cost is one move's
    /// worth of layout however far down the document the view is.
    fn measure_above(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        from: usize,
        amount: f64,
        width: f32,
    ) {
        let from = from.min(self.lines.heights.len());
        let mut line = from;
        while line > 0 {
            let heights = &self.lines.heights;
            if heights.offset_of(from) - heights.offset_of(line) >= amount {
                break;
            }
            // A measured line needs nothing, and the rest of a measured
            // block's lines are measured at zero, so step over them without
            // looking the block up.
            if heights.is_measured(line - 1) {
                line -= 1;
                continue;
            }
            let start = Self::block_start(doc, parse, line - 1).min(line - 1);
            if self.block_unmeasured(start, line) {
                self.store_block(doc, parse, start, width);
            }
            line = start;
        }
    }

    /// Measures from the anchor's block through `extra` points past the
    /// bottom of the viewport. Blocks entirely below that stay estimated.
    fn measure_down(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        width: f32,
        viewport: f32,
        extra: f64,
    ) {
        let anchor = self.lines.anchor;
        if anchor.line >= self.lines.heights.len() {
            return;
        }
        let from = Self::block_start(doc, parse, anchor.line);
        let heights = &self.lines.heights;
        let amount =
            heights.anchor_y(anchor) - heights.offset_of(from) + f64::from(viewport) + extra;
        self.measure_below(doc, parse, from, amount, width);
    }

    /// Lays the visible blocks out again so a caret reveal matches paint.
    fn remeasure_visible(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        width: f32,
        viewport: f32,
    ) {
        let count = doc.line_count().min(self.lines.heights.len());
        if count == 0 {
            return;
        }
        let mut line = self.lines.anchor.line.min(count - 1);
        if let Some((first, _)) = leaf_lines(doc, parse, line) {
            line = first;
        }
        let mut steps = 0;
        while line < count && steps <= count {
            steps += 1;
            let limit = self.lines.heights.anchor_y(self.lines.anchor) + f64::from(viewport);
            if line > self.lines.anchor.line && self.lines.heights.offset_of(line) >= limit {
                break;
            }
            let next = self.store_block(doc, parse, line, width);
            if next <= line {
                break;
            }
            line = next;
        }
    }

    /// Measures what the view is about to show, then snaps the anchor the
    /// way paint does, so paint's own measurement does not move the view.
    /// `extra` is the scroll about to be applied: below the viewport when
    /// positive, above the anchor when negative, so every block it passes
    /// through has its real height before it can move the view. Blocks
    /// above the anchor that a scroll doesn't reach may stay estimated: the
    /// anchor is a line, so measuring them later can't shove the view.
    fn settle(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        width: f32,
        viewport: f32,
        extra: f64,
    ) {
        if self.lines.heights.is_empty() {
            return;
        }
        self.remeasure_edited(doc, parse, width, viewport);
        self.collapse_anchor_block(doc, parse, width);
        if extra < 0.0 {
            let from = Self::block_start(doc, parse, self.lines.anchor.line);
            self.measure_above(doc, parse, from, -extra, width);
        }
        self.measure_down(doc, parse, width, viewport, extra.max(0.0));
        self.remeasure_visible(doc, parse, width, viewport);
        let y = self.lines.heights.anchor_y(self.lines.anchor);
        self.lines.anchor = self.lines.heights.line_at(y);
        let line = self.lines.anchor.line;
        if line < self.lines.heights.len() && !self.lines.heights.is_measured(line) {
            self.store_block(doc, parse, line, width);
            self.remeasure_visible(doc, parse, width, viewport);
            let y = self.lines.heights.anchor_y(self.lines.anchor);
            self.lines.anchor = self.lines.heights.line_at(y);
        }
    }

    /// Measures the caret's block and a viewport's worth of blocks above
    /// it, so a caret above or below the view has real heights for
    /// everything the view will show once it moves there.
    fn measure_around_line(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        line: usize,
        width: f32,
        viewport: f32,
    ) {
        let count = doc.line_count().min(self.lines.heights.len());
        if count == 0 {
            return;
        }
        let first = Self::block_start(doc, parse, line.min(count - 1));
        let end = leaf_lines(doc, parse, first).map_or(first + 1, |(_, last)| last + 1);
        if self.block_unmeasured(first, end) {
            self.store_block(doc, parse, first, width);
        }
        self.measure_above(doc, parse, first, f64::from(viewport), width);
    }

    /// Scrollbar, the text column's right edge, and the minimap rect.
    fn pane_bounds(&self, rect: Rect) -> (Rect, f32, Option<Rect>) {
        let bar = Rect::from_min_max(pos2(rect.right() - SCROLLBAR_WIDTH, rect.top()), rect.max);
        let text_right = if self.show_minimap {
            bar.left() - inkmark_minimap::WIDTH
        } else {
            bar.left()
        };
        let minimap = self.show_minimap.then(|| {
            Rect::from_min_max(
                pos2(text_right, rect.top()),
                pos2(bar.left(), rect.bottom()),
            )
        });
        (bar, text_right, minimap)
    }

    /// Wheel, scrollbar, and minimap for the live pane. A wheel measures the
    /// lines it is about to reveal, and every path measures the viewport
    /// again before paint.
    fn reading_scroll(
        &mut self,
        ui: &Ui,
        doc: &Document,
        parse: &ParseOutput,
        response: &Response,
        frame: Frame,
    ) -> bool {
        let (bar, _, minimap) = self.pane_bounds(frame.rect);
        let viewport = frame.rect.height();
        let width = frame.width;
        let hovered = response.hovered();
        let mut scrolled = false;
        let mut minimap_hovered = false;
        if let Some(rect) = minimap {
            let hit = ui.interact(rect, self.id.with("minimap"), Sense::click_and_drag());
            lines::keep_focus(ui, self.id, &hit);
            minimap_hovered = hit.hovered();
            let pressed =
                hit.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed());
            let map = self.lines.minimap(rect, viewport);
            // The minimap sits outside the text area, so the drag is `hit`'s.
            let target = match hit.interact_pointer_pos() {
                Some(p) if pressed => Some(map.jump_target(p.y)),
                Some(p) if hit.dragged() => Some(map.drag_target(p.y)),
                _ => None,
            };
            if let Some(top) = target {
                self.lines.anchor = self.lines.heights.line_at(top);
                self.lines.anchor = self
                    .lines
                    .heights
                    .scroll_by(self.lines.anchor, 0.0, viewport);
                scrolled = true;
            }
        }
        let bar_response = ui.interact(bar, self.id.with("scrollbar"), Sense::click_and_drag());
        lines::keep_focus(ui, self.id, &bar_response);
        if (bar_response.dragged() || bar_response.clicked())
            && let Some(pos) = bar_response.interact_pointer_pos()
        {
            let frac = ((pos.y - bar.top()) / bar.height()).clamp(0.0, 1.0);
            let target = f64::from(frac) * self.lines.heights.total() - f64::from(viewport) / 2.0;
            self.lines.anchor = self.lines.heights.line_at(target.max(0.0));
            self.lines.anchor = self
                .lines
                .heights
                .scroll_by(self.lines.anchor, 0.0, viewport);
            scrolled = true;
        }
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if (hovered || bar_response.hovered() || minimap_hovered) && wheel != 0.0 {
            let delta = -wheel;
            self.settle(doc, parse, width, viewport, f64::from(delta));
            self.lines.anchor = self
                .lines
                .heights
                .scroll_by(self.lines.anchor, delta, viewport);
            scrolled = true;
        }
        self.settle(doc, parse, width, viewport, 0.0);
        scrolled
    }

    /// What raw syntax to show: around the caret, while focused.
    fn reveal(&self, doc: &Document) -> Option<Reveal> {
        self.focused.then(|| reveal_at(doc, self.selection.head))
    }

    /// `[label]`, or as much of the label as fits in `width` followed by
    /// an ellipsis.
    fn fit_marker(&mut self, label: &str, width: f32) -> String {
        let fits =
            |this: &mut Self, text: &str| this.text.geometry(text).caret_x(0, text.len()) <= width;
        let full = format!("[{label}]");
        if fits(self, &full) {
            return full;
        }
        let chars: Vec<char> = label.chars().collect();
        for n in (1..chars.len()).rev() {
            let short = format!("[{}…]", chars[..n].iter().collect::<String>());
            if fits(self, &short) {
                return short;
            }
        }
        "[…]".into()
    }

    fn paint_for(&self, wrap: f32) -> crate::math::Paint {
        crate::math::Paint {
            wrap,
            color: self.theme.text,
            row: self.text.row_height().max(self.line_height).max(1.0),
            ppp: self.text.pixels_per_point().max(0.01),
        }
    }

    /// Build the leaf, then size each formula. A formula that does not parse
    /// is built again as its source. Failed ranges are only added, never dropped.
    fn prepare_layout(
        &mut self,
        doc: &Document,
        map: &SourceMap,
        leaf: &Leaf,
        reveal: Option<Reveal>,
        wrap: f32,
        math_caret: Option<usize>,
    ) -> LeafLayout {
        let paint = self.paint_for(wrap);
        let mut shown: Vec<Range<usize>> = Vec::new();
        for _ in 0..4 {
            let mut layout = live_layout::build(
                doc,
                map,
                leaf,
                reveal.clone(),
                &self.theme,
                &shown,
                math_caret,
            );
            let failed =
                crate::math::MathCache::fit(&mut self.math, &mut self.text, &mut layout, &paint);
            if failed.is_empty() {
                return layout;
            }
            let before = shown.len();
            for src in failed {
                if !shown.contains(&src) {
                    shown.push(src);
                }
            }
            if shown.len() == before {
                return layout;
            }
        }
        live_layout::build(doc, map, leaf, reveal, &self.theme, &shown, math_caret)
    }

    /// Shape `layout` at `wrap`. Segment tops start at `origin`. When `count`
    /// is false the tops stay at `origin`: an image-only block hides its text.
    fn shape_segments(
        &mut self,
        layout: &LeafLayout,
        wrap: f32,
        origin: f32,
        count: bool,
    ) -> (Vec<f32>, Vec<LineGeometry>, f32) {
        let paint = self.paint_for(wrap);
        let mut y = origin;
        let mut seg_tops = Vec::with_capacity(layout.segments.len());
        let mut geometry = Vec::with_capacity(layout.segments.len());
        for seg in &layout.segments {
            let mut g = self.text.rich_geometry(RichLine {
                text: &seg.text,
                runs: &seg.runs,
                wrap_width: Some(wrap),
            });
            if let Some(math) = seg.maths.iter().find(|m| m.display_style)
                && let Some(size) = self.math.size(&math.tex, true, &paint)
            {
                crate::math::raise_display(&mut g, size, wrap);
            }
            seg_tops.push(y);
            if count {
                y += g.height();
            }
            geometry.push(g);
        }
        (seg_tops, geometry, y)
    }

    fn place(&mut self, doc: &Document, parse: &ParseOutput, leaf: Leaf, width: f32) -> Placed {
        let (map, link_defs) = (&parse.map, &parse.link_defs);
        let containers: f32 = leaf.containers.iter().map(container_indent).sum();
        let row = self.text.row_height();
        let style = match leaf.block.kind {
            BlockKind::Heading(level) => LeafStyle::Heading(level),
            BlockKind::CodeBlock { .. } => LeafStyle::Code,
            BlockKind::HtmlBlock => LeafStyle::Html,
            BlockKind::ThematicBreak => LeafStyle::Rule,
            _ => LeafStyle::Paragraph,
        };
        let (pad_top, pad_bottom, inner) = match style {
            LeafStyle::Heading(level) => {
                (row * if level <= 2 { 0.7 } else { 0.45 }, row * 0.25, 0.0)
            }
            LeafStyle::Code | LeafStyle::Html => (CODE_PAD, CODE_PAD, CODE_PAD),
            LeafStyle::Paragraph | LeafStyle::Rule => (0.0, 0.0, 0.0),
        };
        let range = leaf.block.range.clone();
        let first_line = doc.byte_to_line(range.start);
        let last_line = doc.byte_to_line(range.end.saturating_sub(1).max(range.start));

        // A block with a very long line is drawn as a notice (see
        // `live_layout::long_notice`); its spans would only be scanned here.
        let spans = if crate::long_line::holds_long_line(doc, &range) {
            Vec::new()
        } else {
            map.spans_in(range.clone())
        };
        let has_image = spans.iter().any(|s| s.style.contains(Style::IMAGE));
        let image_only = has_image
            && spans.iter().all(|s| {
                s.style.contains(Style::IMAGE)
                    || matches!(
                        s.kind,
                        SpanKind::Whitespace
                            | SpanKind::SoftBreak
                            | SpanKind::Syntax(Syntax::QuotePrefix | Syntax::ListMarker)
                    )
            });
        let caret_line = doc.byte_to_line(self.selection.head);
        let text_hidden =
            image_only && !(self.focused && (first_line..=last_line).contains(&caret_line));

        let wrap = (width - containers - 2.0 * inner).max(40.0);
        let layout = self.prepare_layout(
            doc,
            map,
            &leaf,
            self.reveal(doc),
            wrap,
            Some(self.selection.head),
        );
        let (seg_tops, geometry, mut y) = self.shape_segments(&layout, wrap, pad_top, !text_hidden);

        let mut images = Vec::new();
        if has_image && let Some(cache) = &self.images {
            let base = doc.path().and_then(|p| p.parent());
            for image in inline_images(&doc.slice(range.clone()), link_defs) {
                let slot = cache.get(&image.dest, base);
                let size = match &slot {
                    ImageSlot::Ready { size, .. } if size.x > wrap => *size * (wrap / size.x),
                    ImageSlot::Ready { size, .. } => *size,
                    ImageSlot::Loading => vec2(wrap.min(320.0), 120.0),
                    ImageSlot::Failed(_) | ImageSlot::Remote => vec2(wrap, row * 2.0),
                };
                if !(text_hidden && images.is_empty()) {
                    y += IMAGE_GAP;
                }
                images.push(PlacedImage {
                    top: y,
                    size,
                    slot,
                    dest: image.dest,
                });
                y += size.y;
            }
        }
        let body = Body {
            layout,
            seg_tops,
            geometry,
            range: range.clone(),
            wrap,
        };
        let (table, height) = if matches!(leaf.block.kind, BlockKind::Table { .. }) {
            let grid = self.place_table(doc, parse, &leaf.block, width - containers);
            let h = grid.height() + 2.0 * TABLE_PAD;
            (Some(grid), h)
        } else {
            (None, y + pad_bottom)
        };
        Placed {
            first_line,
            last_line,
            indent: containers + inner,
            height,
            leaf,
            body,
            table,
            text_hidden,
            images,
        }
    }

    /// Lays a GFM table out as a grid: each cell like a small paragraph,
    /// columns at their natural width, shrunk (and wrapped) to fit `avail`.
    fn place_table(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        table: &inkmark_parse::Block,
        avail: f32,
    ) -> Grid {
        let rows = parse.blocks.table_rows(table);
        let declared = match table.kind {
            BlockKind::Table { columns } => usize::from(columns),
            _ => 0,
        };
        let columns = rows
            .iter()
            .map(|r| r.1.len())
            .max()
            .unwrap_or(0)
            .max(declared)
            .max(1);
        let aligns = table_alignments(doc, &rows, columns);
        let reveal = self.reveal(doc);
        let table_cells: Vec<Vec<inkmark_parse::Block>> = rows
            .iter()
            .map(|(_, row)| row.iter().take(columns).cloned().collect())
            .collect();
        // Each column's widest cell unwrapped (max) and its longest word
        // (min). Columns get their max if everything fits; otherwise the
        // space above the minimums is shared in proportion, so short
        // columns aren't broken mid-word to make room for long ones.
        // Formulas are sized at this width, so a formula cell is as wide as
        // its drawing rather than the one stand-in character.
        let mut max_w = vec![MIN_COLUMN; columns];
        let mut min_w = vec![MIN_COLUMN; columns];
        for row in &table_cells {
            for (c, cell) in row.iter().enumerate() {
                let body = self.lay_cell(doc, parse, cell, reveal.clone(), 100_000.0);
                // A little slack: wrapping at exactly the measured width can
                // still break the line after pixel rounding.
                let width = body.width() + 2.0;
                max_w[c] = max_w[c].max(width + 2.0 * CELL_PAD_X);
                let word = body
                    .layout
                    .segments
                    .iter()
                    .flat_map(|seg| seg.text.split_whitespace())
                    .map(|w| {
                        self.text.geometry(w).rows[0]
                            .clusters
                            .last()
                            .map_or(0.0, |c| c.x + c.w)
                    })
                    .fold(0.0, f32::max);
                // Bold header text is a bit wider than the plain measure.
                min_w[c] = min_w[c]
                    .max(word * 1.1 + 2.0 + 2.0 * CELL_PAD_X)
                    .min(max_w[c]);
            }
        }
        let (sum_max, sum_min): (f32, f32) = (max_w.iter().sum(), min_w.iter().sum());
        let col_w: Vec<f32> = if sum_max <= avail {
            max_w
        } else if sum_min >= avail {
            min_w
        } else {
            let share = (avail - sum_min) / (sum_max - sum_min);
            min_w
                .iter()
                .zip(&max_w)
                .map(|(lo, hi)| lo + (hi - lo) * share)
                .collect()
        };
        let col_x: Vec<f32> = col_w
            .iter()
            .scan(0.0, |x, w| {
                let at = *x;
                *x += w;
                Some(at)
            })
            .collect();
        let row_min = self.text.row_height() + 2.0 * CELL_PAD_Y;
        let mut cells = Vec::new();
        let mut row_y = Vec::new();
        let mut row_h = Vec::new();
        let mut y = 0.0;
        for row in &table_cells {
            let mut h = row_min;
            let start = cells.len();
            for (c, cell) in row.iter().enumerate() {
                let inner = col_w[c] - 2.0 * CELL_PAD_X;
                let body = self.lay_cell(doc, parse, cell, reveal.clone(), inner.max(8.0));
                let text_h = body.seg_tops.last().copied().unwrap_or(0.0)
                    + body.geometry.last().map_or(0.0, |g| g.height());
                h = h.max(text_h + 2.0 * CELL_PAD_Y);
                let slack = (inner - body.width()).max(0.0);
                let dx = match aligns[c] {
                    Align::Left => 0.0,
                    Align::Center => slack / 2.0,
                    Align::Right => slack,
                };
                cells.push(GridCell {
                    body,
                    origin: vec2(col_x[c] + CELL_PAD_X + dx, y + CELL_PAD_Y),
                });
            }
            for cell in &mut cells[start..] {
                cell.origin.y = y + CELL_PAD_Y;
            }
            row_y.push(y);
            row_h.push(h);
            y += h;
        }
        Grid {
            cells,
            col_x,
            col_w,
            row_y,
            row_h,
            has_head: rows
                .first()
                .is_some_and(|r| r.0.kind == BlockKind::TableHead),
        }
    }

    fn lay_cell(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        cell: &inkmark_parse::Block,
        reveal: Option<Reveal>,
        wrap: f32,
    ) -> Body {
        let leaf = Leaf {
            block: cell.clone(),
            containers: Vec::new(),
        };
        let layout = self.prepare_layout(
            doc,
            &parse.map,
            &leaf,
            reveal,
            wrap,
            Some(self.selection.head),
        );
        let (seg_tops, geometry, _) = self.shape_segments(&layout, wrap, 0.0, true);
        Body {
            layout,
            seg_tops,
            geometry,
            range: cell.range.clone(),
            wrap,
        }
    }

    fn place_line(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        line: usize,
        width: f32,
    ) -> Option<Placed> {
        let leaf = leaf_at_line(doc, parse, line)?;
        Some(self.place(doc, parse, leaf, width))
    }

    // ---- caret -----------------------------------------------------------------

    /// The caret in document coordinates: (top y, left x from content left, height).
    fn caret_doc(&mut self, doc: &Document, parse: &ParseOutput, frame: Frame) -> (f64, f32, f32) {
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        match self.place_line(doc, parse, line, frame.width) {
            Some(p) => {
                let r = p.caret_rect(head, self.upstream());
                let top = self.lines.heights.offset_of(p.first_line) + f64::from(r.top());
                (top, p.indent + r.left(), r.height())
            }
            None => (
                self.lines.heights.offset_of(line),
                0.0,
                self.text.row_height(),
            ),
        }
    }

    /// The source offset at document height `y`, content x `x`.
    fn offset_at_doc(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        frame: Frame,
        y: f64,
        x: f32,
    ) -> usize {
        let at = self.lines.heights.line_at(y.max(0.0));
        match self.place_line(doc, parse, at.line, frame.width) {
            Some(p) => {
                let local = (y - self.lines.heights.offset_of(p.first_line)) as f32;
                let (at, upstream) = p.hit(vec2(x - p.indent, local));
                self.hit_upstream = upstream;
                at
            }
            None => {
                self.hit_upstream = false;
                doc.line_to_byte(at.line)
            }
        }
    }

    fn offset_at(&mut self, doc: &Document, parse: &ParseOutput, frame: Frame, pos: Pos2) -> usize {
        let y =
            self.lines.heights.anchor_y(self.lines.anchor) + f64::from(pos.y - frame.rect.top());
        self.offset_at_doc(doc, parse, frame, y, pos.x - frame.left)
    }

    /// Whether `offset` is a place the caret can show at, with the reveal
    /// state it would have there.
    fn is_visible(&self, doc: &Document, parse: &ParseOutput, offset: usize) -> bool {
        let line = doc.byte_to_line(offset);
        let Some(mut leaf) = leaf_at_line(doc, parse, line) else {
            return true;
        };
        if matches!(leaf.block.kind, BlockKind::Table { .. }) {
            // In a table only places inside a cell's text are stops; pipes,
            // padding and the delimiter row aren't.
            let cell = parse
                .blocks
                .table_rows(&leaf.block)
                .into_iter()
                .flat_map(|(_, cells)| cells)
                .find(|c| !c.range.is_empty() && c.range.start <= offset && offset <= c.range.end);
            let Some(cell) = cell else {
                return false;
            };
            leaf = Leaf {
                block: cell,
                containers: Vec::new(),
            };
        }
        let layout = live_layout::build(
            doc,
            &parse.map,
            &leaf,
            Some(reveal_at(doc, offset)),
            &self.theme,
            &[],
            None,
        );
        let (seg, d) = layout.display_pos(offset);
        layout.source_pos(seg, d) == offset
    }

    /// One step left or right (by grapheme or word), skipping hidden bytes.
    ///
    /// A long-line notice (see `live_layout::long_notice`) is crossed whole
    /// by a move, without reading the source it stands for. A selection
    /// stops at its edge rather than taking the whole block, and a delete at
    /// its edge takes one grapheme (or word) of the source, as the code pane
    /// would.
    fn step(
        &self,
        doc: &Document,
        parse: &ParseOutput,
        forward: bool,
        word: bool,
        purpose: Step,
    ) -> usize {
        let one = |at| match (forward, word) {
            (true, false) => motion::next_grapheme(doc, at),
            (true, true) => motion::next_word(doc, at),
            (false, false) => motion::prev_grapheme(doc, at),
            (false, true) => motion::prev_word(doc, at),
        };
        let head = self.selection.head;
        // The edge facing the step, and the one across.
        let edges = |block: Range<usize>| {
            if forward {
                (block.start, block.end)
            } else {
                (block.end, block.start)
            }
        };
        if let Some(block) = notice_at(doc, parse, head) {
            let (near, far) = edges(block);
            if head == near {
                return match purpose {
                    Step::Move => far,
                    Step::Select => head,
                    Step::Delete => one(head),
                };
            }
        }
        let mut at = head;
        for _ in 0..256 {
            let next = one(at);
            if next == at {
                break;
            }
            at = next;
            // Into a notice from outside it (a word step can): to the edge
            // across for a move, to the edge crossed otherwise.
            if let Some(block) = notice_at(doc, parse, at).filter(|b| b.start < at && at < b.end) {
                let (near, far) = edges(block);
                at = if purpose == Step::Move { far } else { near };
                break;
            }
            if self.is_visible(doc, parse, at) {
                break;
            }
        }
        at
    }

    /// Moves an end of the selection that lies inside a long-line notice
    /// to the notice's end, where it is drawn, so an edit happens where the
    /// caret shows. (The code pane can leave it anywhere in the line.)
    fn settle_in_notice(&mut self, doc: &Document, parse: &ParseOutput) {
        let settle = |at: usize| match notice_at(doc, parse, at) {
            Some(block) if block.start < at && at < block.end => block.end,
            _ => at,
        };
        let (anchor, head) = (settle(self.selection.anchor), settle(self.selection.head));
        if (anchor, head) != (self.selection.anchor, self.selection.head) {
            self.selection = Selection { anchor, head };
            self.preferred_x = None;
        }
    }

    fn move_to(&mut self, target: usize, extend: bool) {
        if extend {
            self.selection.head = target;
        } else {
            self.selection = Selection::caret(target);
        }
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
        // Moving the caret ends the typing group, even if it comes back to
        // the same spot (the code pane does the same).
        self.seal_undo = true;
    }

    fn move_vertical(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        frame: Frame,
        dy: f32,
        extend: bool,
    ) {
        // The caret moves by document height, so its own block and whatever
        // it crosses need their real heights first: the caret's y comes from
        // the block's layout, and the target is found in the height cache.
        // One move's worth, never the whole prefix.
        let caret_line = doc.byte_to_line(self.selection.head.min(doc.len()));
        let first = Self::block_start(doc, parse, caret_line);
        let end = leaf_lines(doc, parse, first).map_or(first + 1, |(_, last)| last + 1);
        if self.block_unmeasured(first, end) {
            self.store_block(doc, parse, first, frame.width);
        }
        let reach = f64::from(dy.abs() + self.text.row_height());
        if dy < 0.0 {
            self.measure_above(doc, parse, first, reach, frame.width);
        } else {
            let heights = &self.lines.heights;
            let block = heights.offset_of(end.min(heights.len())) - heights.offset_of(first);
            self.measure_below(doc, parse, first, block + reach, frame.width);
        }
        let (top, x, height) = self.caret_doc(doc, parse, frame);
        let x = self.preferred_x.unwrap_or(x);
        let mut y = if dy < 0.0 {
            top - 1.0 + f64::from(dy + 1.0).min(0.0)
        } else {
            top + f64::from(height) + 1.0 + f64::from(dy - 1.0).max(0.0)
        };
        // Padding around headings and code blocks maps back to the same row;
        // creep on in small steps (blank lines are short) until the caret moves.
        let mut target = self.selection.head;
        for _ in 0..32 {
            target = if y < 0.0 {
                0
            } else if y >= self.lines.heights.total() {
                doc.len()
            } else {
                self.offset_at_doc(doc, parse, frame, y, x)
            };
            if target != self.selection.head {
                break;
            }
            y += 4.0f64.copysign(f64::from(dy));
        }
        // Into a long-line notice from outside it: on the edge facing the
        // caret, whichever of its glyphs is under x, as the code pane enters
        // the line's first or last row.
        let head = self.selection.head;
        if let Some(block) = notice_at(doc, parse, target)
            && !block.contains(&head)
            && head != block.end
        {
            target = if dy < 0.0 { block.end } else { block.start };
        }
        let upstream = self.hit_upstream && target != 0 && target != doc.len();
        self.move_to(target, extend);
        self.upstream_at = upstream.then_some(target);
        self.preferred_x = Some(x);
    }

    fn upstream(&self) -> bool {
        self.upstream_at == Some(self.selection.head)
    }

    /// Start or end of the caret's visual row, and whether that end is
    /// upstream (see `upstream_at`).
    fn row_edge(
        &mut self,
        doc: &Document,
        parse: &ParseOutput,
        frame: Frame,
        end: bool,
    ) -> (usize, bool) {
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        let upstream = self.upstream();
        let Some(p) = self.place_line(doc, parse, line, frame.width) else {
            return (doc.line_to_byte(line), false);
        };
        p.row_edge(head, end, upstream)
    }

    // ---- input ---------------------------------------------------------------

    /// Keyboard, clipboard and IME input. Each edit is re-parsed before
    /// the next event, so navigation never reads a stale map.
    fn handle_events(&mut self, ui: &Ui, doc: &mut Document, state: &mut ParseState, frame: Frame) {
        let events = ui.input(|i| i.events.clone());
        for event in events {
            let parse = state.output();
            if !matches!(event, Event::Copy) {
                self.settle_in_notice(doc, parse);
            }
            let edited = match event {
                Event::Copy | Event::Cut => {
                    let range = self.selection.range();
                    if range.is_empty() {
                        false
                    } else {
                        ui.ctx().copy_text(doc.slice(range.clone()).into_owned());
                        matches!(event, Event::Cut) && self.delete(doc, range, EditKind::Other)
                    }
                }
                Event::Paste(text) => {
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    let text = self.fit_to_cell(doc, parse, text);
                    self.replace_selection(doc, &text, EditKind::Other)
                }
                Event::Text(text) if self.preedit.is_empty() => {
                    let text = self.fit_to_cell(doc, parse, text);
                    self.replace_selection(doc, &text, EditKind::Typing)
                }
                Event::Ime(ImeEvent::Preedit { text, .. }) => {
                    let starting = self.preedit.is_empty() && !text.is_empty();
                    self.preedit = text;
                    self.reveal_caret = REVEAL_FRAMES;
                    let range = self.selection.range();
                    starting && self.delete(doc, range, EditKind::Other)
                }
                Event::Ime(ImeEvent::Commit(text)) => {
                    self.preedit.clear();
                    let text = self.fit_to_cell(doc, parse, text);
                    self.replace_selection(doc, &text, EditKind::Typing)
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if self.preedit.is_empty() => self.handle_key(doc, parse, frame, key, modifiers),
                _ => false,
            };
            if edited && let Some(wait) = state.update(doc) {
                ui.ctx().request_repaint_after(wait);
            }
        }
    }

    // ---- editing --------------------------------------------------------------

    fn after_edit(&mut self, doc: &Document) {
        self.sync(doc, false);
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
    }

    fn apply_plan(&mut self, doc: &mut Document, plan: EditPlan) -> bool {
        if std::mem::take(&mut self.seal_undo) {
            doc.seal_undo_step();
        }
        if plan.edits.is_empty()
            || doc
                .apply(plan.edits, self.selection, plan.selection, plan.kind)
                .is_err()
        {
            return false;
        }
        self.selection = plan.selection;
        self.after_edit(doc);
        true
    }

    /// Right-click: the caret moves to the pointer (unless it's inside the
    /// selection), then a menu offers table commands. Returns whether an
    /// edit was made.
    fn context_menu(
        &mut self,
        response: &Response,
        doc: &mut Document,
        state: &mut ParseState,
        frame: Frame,
    ) -> bool {
        if response.secondary_clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let at = self.offset_at(doc, state.output(), frame, pos);
            if !self.selection.range().contains(&at) {
                self.move_to(at, false);
            }
        }
        let in_table = tables::table_start(doc, state.output(), self.selection.head).is_some();
        let mut chosen = None;
        response.context_menu(|ui| {
            let mut item = |ui: &mut Ui, label: &str, hint: &str, command| {
                if ui
                    .add(egui::Button::new(label).shortcut_text(hint))
                    .clicked()
                {
                    chosen = Some(command);
                    ui.close();
                }
            };
            use tables::{Align, TableCommand::*, TableOp::*};
            let hint = |action: Action| self.keys.shortcut_text(action);
            if in_table {
                item(
                    ui,
                    "Insert row above",
                    &hint(Action::InsertRowAbove),
                    Edit(InsertRowAbove),
                );
                item(
                    ui,
                    "Insert row below",
                    &hint(Action::InsertRowBelow),
                    Edit(InsertRowBelow),
                );
                item(
                    ui,
                    "Insert column left",
                    &hint(Action::InsertColumnLeft),
                    Edit(InsertColumnLeft),
                );
                item(
                    ui,
                    "Insert column right",
                    &hint(Action::InsertColumnRight),
                    Edit(InsertColumnRight),
                );
                ui.separator();
                item(ui, "Delete row", &hint(Action::DeleteRow), Edit(DeleteRow));
                item(
                    ui,
                    "Delete column",
                    &hint(Action::DeleteColumn),
                    Edit(DeleteColumn),
                );
                ui.separator();
                item(ui, "Move row up", &hint(Action::MoveRowUp), Edit(MoveRowUp));
                item(
                    ui,
                    "Move row down",
                    &hint(Action::MoveRowDown),
                    Edit(MoveRowDown),
                );
                item(
                    ui,
                    "Move column left",
                    &hint(Action::MoveColumnLeft),
                    Edit(MoveColumnLeft),
                );
                item(
                    ui,
                    "Move column right",
                    &hint(Action::MoveColumnRight),
                    Edit(MoveColumnRight),
                );
                ui.separator();
                ui.menu_button("Align column", |ui| {
                    item(ui, "Left", "", Edit(Align(Align::Left)));
                    item(ui, "Center", "", Edit(Align(Align::Center)));
                    item(ui, "Right", "", Edit(Align(Align::Right)));
                    item(ui, "None", "", Edit(Align(Align::None)));
                });
                item(ui, "Format table", &hint(Action::FormatTable), Edit(Format));
            } else {
                item(ui, "Insert table", &hint(Action::InsertTable), Insert);
            }
        });
        let Some(command) = chosen else {
            return false;
        };
        let (plan, _) = tables::run(doc, state.output(), self.selection.head, command);
        let Some(plan) = plan else { return false };
        let edited = self.apply_plan(doc, plan);
        if edited {
            state.update(doc);
        }
        edited
    }

    /// Notices the caret leaving a table whose text it changed (edits
    /// undone don't count) and re-pads that table, as an undo step of its
    /// own. A table only moved through is left as it is.
    fn repad_left_table(&mut self, doc: &mut Document, state: &mut ParseState) {
        // Only while this pane has the keyboard: a caret mirrored from the
        // code pane, and edits made there, aren't the live pane's.
        if !self.focused {
            self.table_visit = None;
            return;
        }
        let parse = state.output();
        let here = tables::table_start(doc, parse, self.selection.head);
        let visit = self.table_visit.take();
        let enter = |doc: &Document, start: usize| {
            tables::table_text(doc, parse, start).map(|text| (start, text))
        };
        let Some((start, entered)) = visit else {
            self.table_visit = here.and_then(|s| enter(doc, s));
            return;
        };
        if here == Some(start) {
            self.table_visit = Some((start, entered));
            return;
        }
        let changed = tables::table_text(doc, parse, start).is_some_and(|now| now != entered);
        let format = if changed {
            tables::format_table(doc, parse, start)
        } else {
            None
        };
        if let Some((edit, grew)) = format {
            let table_end = edit.range.end;
            let shift = |at: usize| {
                if at >= table_end {
                    (at as isize + grew).max(0) as usize
                } else {
                    at
                }
            };
            let after = Selection {
                anchor: shift(self.selection.anchor),
                head: shift(self.selection.head),
            };
            doc.seal_undo_step();
            if doc
                .apply(vec![edit], self.selection, after, EditKind::Other)
                .is_ok()
            {
                doc.seal_undo_step();
                self.selection = after;
                self.after_edit(doc);
                state.update(doc);
            }
        }
        let parse = state.output();
        self.table_visit = tables::table_start(doc, parse, self.selection.head)
            .and_then(|s| tables::table_text(doc, parse, s).map(|t| (s, t)));
    }

    /// Replaces the selection with `text` (a source patch of exactly that).
    fn replace_selection(&mut self, doc: &mut Document, text: &str, kind: EditKind) -> bool {
        let range = self.selection.range();
        if range.is_empty() && text.is_empty() {
            return false;
        }
        let caret = range.start + text.len();
        self.apply_plan(
            doc,
            EditPlan {
                edits: vec![Edit::replace(range, text)],
                selection: Selection::caret(caret),
                kind,
            },
        )
    }

    fn delete(&mut self, doc: &mut Document, range: Range<usize>, kind: EditKind) -> bool {
        if range.is_empty() {
            return false;
        }
        let caret = range.start;
        self.apply_plan(
            doc,
            EditPlan {
                edits: vec![Edit::delete(range)],
                selection: Selection::caret(caret),
                kind,
            },
        )
    }

    fn undo(&mut self, doc: &mut Document, redo: bool) -> bool {
        let selection = if redo { doc.redo() } else { doc.undo() };
        let Some(selection) = selection else {
            return false;
        };
        self.selection = selection;
        self.after_edit(doc);
        true
    }

    /// The table around the caret: its rows (with cells), and the caret's
    /// row and column. `None` outside tables and on the delimiter row.
    fn table_at_caret(&self, doc: &Document, parse: &ParseOutput) -> Option<TableAt> {
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        let leaf = leaf_at_line(doc, parse, line)?;
        if !matches!(leaf.block.kind, BlockKind::Table { .. }) {
            return None;
        }
        let mut rows = parse.blocks.table_rows(&leaf.block);
        // pulldown-cmark pads short rows with zero-width cells at the next
        // line's start; they have no text to visit.
        for (_, cells) in &mut rows {
            cells.retain(|c| !c.range.is_empty());
        }
        let row = rows
            .iter()
            .position(|(r, _)| doc.byte_to_line(r.range.start) == line)?;
        let col = rows[row]
            .1
            .iter()
            .rposition(|c| c.range.start <= head)
            .unwrap_or(0);
        Some(TableAt { rows, row, col })
    }

    /// The editable text of the table cell holding the caret (its range
    /// without padding), or `None` outside tables.
    fn cell_text_range(&self, doc: &Document, parse: &ParseOutput) -> Option<Range<usize>> {
        let t = self.table_at_caret(doc, parse)?;
        let cell = t.rows[t.row].1.get(t.col)?;
        let text = doc.slice(cell.range.clone());
        let start = cell.range.start + (text.len() - text.trim_start().len());
        let end = cell.range.end - (text.len() - text.trim_end().len());
        Some(start..end.max(start))
    }

    /// Text going into a table cell can't split it: `|` is escaped and line
    /// breaks become spaces. Elsewhere `text` is unchanged.
    fn fit_to_cell(&self, doc: &Document, parse: &ParseOutput, text: String) -> String {
        if self.table_at_caret(doc, parse).is_none() {
            return text;
        }
        text.replace('|', "\\|").replace('\n', " ")
    }

    /// Tab / Shift+Tab / Enter inside a table. `None` when not in one.
    fn table_key(
        &mut self,
        doc: &mut Document,
        parse: &ParseOutput,
        key: Key,
        shift: bool,
    ) -> Option<bool> {
        let t = self.table_at_caret(doc, parse)?;
        let cells: Vec<(usize, usize)> = t
            .rows
            .iter()
            .enumerate()
            .flat_map(|(r, (_, cells))| (0..cells.len()).map(move |c| (r, c)))
            .collect();
        let here = cells.iter().position(|&rc| rc == (t.row, t.col))?;
        let goto = |this: &mut Self, (r, c): (usize, usize)| {
            let at = cell_text_start(doc, &t.rows[r].1[c]);
            this.move_to(at, false);
            false
        };
        Some(match key {
            Key::Tab if shift => here.checked_sub(1).is_some_and(|k| goto(self, cells[k])),
            Key::Tab if here + 1 < cells.len() => goto(self, cells[here + 1]),
            Key::Tab => self.add_table_row(doc, &t),
            // A line break would end the row; Shift+Enter does nothing here.
            Key::Enter if shift => {
                self.hint = Some("A line break would end the table row");
                false
            }
            Key::Enter if t.row + 1 < t.rows.len() => {
                let below = &t.rows[t.row + 1].1;
                if below.is_empty() {
                    false
                } else {
                    goto(self, (t.row + 1, t.col.min(below.len() - 1)))
                }
            }
            Key::Enter => self.add_table_row(doc, &t),
            _ => return None,
        })
    }

    /// Adds an empty row below the caret's and puts the caret in its first cell.
    fn add_table_row(&mut self, doc: &mut Document, t: &TableAt) -> bool {
        let columns = t.rows.iter().map(|r| r.1.len()).max().unwrap_or(1).max(1);
        let row = &t.rows[t.row].0;
        let line = doc.byte_to_line(row.range.end.saturating_sub(1).max(row.range.start));
        let end = doc.line_range(line).end;
        let text = format!("\n|{}", "  |".repeat(columns));
        // After "\n| ", between the first cell's two spaces.
        let caret = end + 3;
        self.apply_plan(
            doc,
            EditPlan {
                edits: vec![Edit::insert(end, text)],
                selection: Selection::caret(caret),
                kind: EditKind::Other,
            },
        )
    }

    /// Whether the caret is in a code or HTML block, where Enter keeps
    /// indentation instead of starting a paragraph.
    fn in_code(&self, doc: &Document, parse: &ParseOutput) -> bool {
        leaf_at_line(doc, parse, doc.byte_to_line(self.selection.head)).is_some_and(|l| {
            matches!(
                l.block.kind,
                BlockKind::CodeBlock { .. } | BlockKind::HtmlBlock
            )
        })
    }

    /// Returns whether the key edited the document.
    fn handle_key(
        &mut self,
        doc: &mut Document,
        parse: &ParseOutput,
        frame: Frame,
        key: Key,
        modifiers: Modifiers,
    ) -> bool {
        let mut from_table = false;
        if let Some((action, extend)) = self.keys.editor_gesture(key, modifiers)
            && let Some(edited) =
                self.run_editor_action(doc, parse, action, extend, &mut from_table)
        {
            return edited;
        }
        // A table chord that doesn't apply keeps the key's ordinary meaning,
        // so Ctrl+Alt+Left still moves by word. An unbound Ctrl chord does
        // not: word motion is itself a binding.
        let word = from_table && modifiers.command;
        let shift = modifiers.shift;
        // Plain Tab and Enter move between table cells. Their Ctrl and Alt
        // chords are actions, so they were already handled above.
        if !modifiers.command
            && !modifiers.alt
            && matches!(key, Key::Tab | Key::Enter)
            && let Some(edited) = self.table_key(doc, parse, key, shift)
        {
            return edited;
        }
        let sel = self.selection;
        let range = sel.range();
        match key {
            Key::Backspace | Key::Delete if !range.is_empty() => {
                return self.delete(doc, range, EditKind::Deleting);
            }
            Key::Backspace => {
                if !word && let Some(plan) = commands::smart_backspace(doc, sel) {
                    return self.apply_plan(doc, plan);
                }
                let start = self.step(doc, parse, false, word, Step::Delete);
                // In a table, deleting stops at the cell's edge: past it are
                // pipes, which would break the row.
                if let Some(text) = self.cell_text_range(doc, parse)
                    && start < text.start
                {
                    return false;
                }
                return self.delete(doc, start..sel.head, EditKind::Deleting);
            }
            Key::Delete => {
                let end = self.step(doc, parse, true, word, Step::Delete);
                if let Some(text) = self.cell_text_range(doc, parse)
                    && end > text.end
                {
                    return false;
                }
                return self.delete(doc, sel.head..end, EditKind::Deleting);
            }
            Key::Enter if shift => return self.apply_plan(doc, commands::hard_break(doc, sel)),
            Key::Enter => {
                let ctx = EnterContext {
                    in_code: self.in_code(doc, parse),
                };
                return self.apply_plan(doc, commands::smart_enter(doc, sel, ctx));
            }
            Key::Tab => {
                if let Some(plan) = commands::indent_list(doc, sel, shift) {
                    return self.apply_plan(doc, plan);
                }
                return !shift && self.replace_selection(doc, "    ", EditKind::Typing);
            }
            Key::ArrowLeft if !shift && !range.is_empty() => self.move_to(range.start, false),
            Key::ArrowRight if !shift && !range.is_empty() => self.move_to(range.end, false),
            Key::ArrowLeft | Key::ArrowRight => {
                let purpose = if shift { Step::Select } else { Step::Move };
                let target = self.step(doc, parse, key == Key::ArrowRight, word, purpose);
                self.move_to(target, shift);
            }
            Key::ArrowUp => self.move_vertical(doc, parse, frame, -1.0, shift),
            Key::ArrowDown => self.move_vertical(doc, parse, frame, 1.0, shift),
            Key::PageUp => self.move_vertical(doc, parse, frame, -frame.rect.height(), shift),
            Key::PageDown => self.move_vertical(doc, parse, frame, frame.rect.height(), shift),
            Key::Home => {
                let (target, _) = self.row_edge(doc, parse, frame, false);
                self.move_to(target, shift);
            }
            Key::End => {
                let (target, upstream) = self.row_edge(doc, parse, frame, true);
                self.move_to(target, shift);
                self.upstream_at = upstream.then_some(target);
            }
            Key::Escape if !range.is_empty() => self.move_to(sel.head, false),
            _ => {}
        }
        false
    }

    /// Runs a chord from the key table. `None` when a table action doesn't
    /// apply, so the key falls through; `from_table` says that's why.
    /// `Some` is whether the document changed (moving the caret does not).
    fn run_editor_action(
        &mut self,
        doc: &mut Document,
        parse: &ParseOutput,
        action: Action,
        extend: bool,
        from_table: &mut bool,
    ) -> Option<bool> {
        if let Some(command) = tables::command(action) {
            return match tables::run(doc, parse, self.selection.head, command) {
                (Some(plan), _) => Some(self.apply_plan(doc, plan)),
                (None, true) => Some(false),
                (None, false) => {
                    *from_table = true;
                    None
                }
            };
        }
        let sel = self.selection;
        let range = sel.range();
        let head = sel.head;
        if let Some(level) = action.heading_level() {
            return Some(self.apply_plan(doc, commands::set_heading(doc, sel, level)));
        }
        match action {
            Action::Undo => Some(self.undo(doc, false)),
            Action::Redo => Some(self.undo(doc, true)),
            Action::Bold => {
                Some(self.apply_plan(doc, commands::toggle_wrap(doc, sel, "**", &["__"])))
            }
            Action::Italic => {
                Some(self.apply_plan(doc, commands::toggle_wrap(doc, sel, "*", &["_"])))
            }
            Action::Code => Some(self.apply_plan(doc, commands::toggle_wrap(doc, sel, "`", &[]))),
            Action::Strikethrough => {
                Some(self.apply_plan(doc, commands::toggle_wrap(doc, sel, "~~", &["~"])))
            }
            Action::Link => Some(self.apply_plan(doc, commands::insert_link(doc, sel))),
            Action::ToggleTask => Some(self.apply_plan(doc, commands::toggle_task(doc, sel))),
            Action::SelectAll => {
                self.selection = Selection {
                    anchor: 0,
                    head: doc.len(),
                };
                Some(false)
            }
            // Ctrl+Left with a selection collapses it, same as Left. The
            // word jump waits until the caret is alone.
            Action::WordLeft | Action::WordRight if !extend && !range.is_empty() => {
                let at = if action == Action::WordLeft {
                    range.start
                } else {
                    range.end
                };
                self.move_to(at, false);
                Some(false)
            }
            Action::WordLeft | Action::WordRight => {
                let purpose = if extend { Step::Select } else { Step::Move };
                let forward = action == Action::WordRight;
                self.move_to(self.step(doc, parse, forward, true, purpose), extend);
                Some(false)
            }
            Action::DeleteWordLeft | Action::DeleteWordRight if !range.is_empty() => {
                Some(self.delete(doc, range, EditKind::Deleting))
            }
            Action::DeleteWordLeft => {
                let start = self.step(doc, parse, false, true, Step::Delete);
                if let Some(text) = self.cell_text_range(doc, parse)
                    && start < text.start
                {
                    return Some(false);
                }
                Some(self.delete(doc, start..head, EditKind::Deleting))
            }
            Action::DeleteWordRight => {
                let end = self.step(doc, parse, true, true, Step::Delete);
                if let Some(text) = self.cell_text_range(doc, parse)
                    && end > text.end
                {
                    return Some(false);
                }
                Some(self.delete(doc, head..end, EditKind::Deleting))
            }
            Action::DocumentStart => {
                self.move_to(0, extend);
                Some(false)
            }
            Action::DocumentEnd => {
                self.move_to(doc.len(), extend);
                Some(false)
            }
            // Structural selection is a code-pane command. Swallow the chord
            // so it does not fall through into typing.
            Action::SelectWord | Action::SelectParagraph | Action::MatchBracket => Some(false),
            _ => None,
        }
    }

    fn handle_pointer(
        &mut self,
        ui: &Ui,
        response: &Response,
        doc: &Document,
        parse: &ParseOutput,
        frame: Frame,
    ) {
        let (pressed, down, pos, shift) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.interact_pos(),
                i.modifiers.shift,
            )
        });
        if !down {
            self.dragging = false;
        }
        let Some(pos) = pos else { return };
        if pressed && response.hovered() {
            let at = self.offset_at(doc, parse, frame, pos);
            self.move_to(at, shift);
            self.upstream_at = self.hit_upstream.then_some(at);
            self.reveal_caret = 0;
            self.dragging = true;
        } else if self.dragging && down {
            let at = self.offset_at(doc, parse, frame, pos);
            self.selection.head = at;
            self.upstream_at = self.hit_upstream.then_some(at);
        }
        if response.double_clicked() {
            let word = motion::word_at(doc, self.selection.head);
            self.selection = Selection {
                anchor: word.start,
                head: word.end,
            };
        }
    }

    /// Scrolls the caret on screen. Returns whether the anchor moved; the
    /// caller measures that new viewport before paint.
    fn scroll_caret_into_view(
        &mut self,
        ui: &Ui,
        doc: &Document,
        parse: &ParseOutput,
        frame: Frame,
    ) -> bool {
        self.reveal_caret -= 1;
        let caret_line = doc.byte_to_line(self.selection.head.min(doc.len()));
        self.measure_around_line(doc, parse, caret_line, frame.width, frame.rect.height());
        let (top, _, height) = self.caret_doc(doc, parse, frame);
        let bottom = top + f64::from(height);
        let viewport = f64::from(frame.rect.height());
        let view_top = self.lines.heights.anchor_y(self.lines.anchor);
        let new_top = if top < view_top {
            Some(top)
        } else if bottom > view_top + viewport {
            Some((bottom - viewport).max(0.0))
        } else {
            None
        };
        let Some(new_top) = new_top else {
            self.reveal_caret = 0;
            return false;
        };
        self.lines.anchor = self.lines.heights.line_at(new_top);
        if self.reveal_caret > 0 {
            ui.ctx().request_repaint();
        }
        true
    }

    // ---- painting ----------------------------------------------------------------

    /// Paints the visible blocks; returns the caret rect if it is on screen.
    fn paint(
        &mut self,
        painter: &egui::Painter,
        doc: &Document,
        parse: &ParseOutput,
        frame: Frame,
    ) -> Option<Rect> {
        let rect = frame.rect;
        self.checkboxes.clear();
        let heights_len = self.lines.heights.len();
        if heights_len == 0 {
            return None;
        }
        // Normalize the anchor so it never sits on a zero-height line.
        let view_top = self.lines.heights.anchor_y(self.lines.anchor);
        self.lines.anchor = self.lines.heights.line_at(view_top);
        let mut line = self.lines.anchor.line;
        if let Some((first, _)) = leaf_lines(doc, parse, line) {
            line = first;
        }
        let mut y = rect.top() + (self.lines.heights.offset_of(line) - view_top) as f32;
        let mut leaves = parse.blocks.leaves_from(doc.line_to_byte(line)).peekable();
        let mut meshes = GlyphMeshes::default();
        let mut caret = None;
        let caret_line = doc.byte_to_line(self.selection.head);
        let selection = self.selection.range();
        let row = self.text.row_height();

        while y < rect.bottom() && line < doc.line_count() {
            let next_first = leaves.peek().map(|l| doc.byte_to_line(l.block.range.start));
            if next_first.is_none_or(|f| f > line) {
                // A line outside any leaf: blank, or raw (e.g. a link definition).
                let text = Self::raw_line_text(doc, line);
                let height = self.raw_line_height(&text);
                let blank = text
                    .trim_start_matches(|c: char| c.is_whitespace() || c == '>')
                    .is_empty();
                if !blank {
                    self.text
                        .draw_line(&mut meshes, &text, pos2(frame.left, y), self.theme.markup);
                }
                self.lines.heights.set_measured(line, height);
                if line == caret_line {
                    caret = Some(Rect::from_min_size(
                        pos2(frame.left, y),
                        vec2(CARET_WIDTH, height.max(row * 0.8)),
                    ));
                }
                y += height;
                line += 1;
                continue;
            }
            let leaf = leaves.next().expect("peeked");
            let p = self.place(doc, parse, leaf, frame.width);
            // A second leaf on this line stacks under the height already there.
            self.record_block(line, p.first_line, p.last_line, p.height);
            self.draw_leaf(painter, &mut meshes, doc, parse, &p, frame, y, &selection);
            if (p.first_line..=p.last_line).contains(&caret_line) {
                let r = p.caret_rect(self.selection.head, self.upstream());
                caret = Some(r.translate(vec2(frame.left + p.indent, y)));
            }
            y += p.height;
            line = p.last_line + 1;
        }
        if let Some(c) = caret
            && !self.preedit.is_empty()
        {
            // Composition text sits over the line, underlined, until committed.
            let preedit = self.preedit.clone();
            let width = self.text.geometry(&preedit).rows[0]
                .clusters
                .last()
                .map_or(0.0, |c| c.x + c.w);
            let bg = Rect::from_min_size(c.min, vec2(width, c.height()));
            painter.rect_filled(bg, 0.0, self.theme.background);
            self.text
                .draw_line(&mut meshes, &preedit, c.min, self.theme.text);
            painter.hline(bg.x_range(), bg.bottom() - 1.0, (1.0, self.theme.caret));
        }
        self.text.end_frame(meshes, painter);
        if self.focused
            && let Some(c) = caret
        {
            painter.rect_filled(c, 0.0, self.theme.caret);
        }
        caret.filter(|c| rect.intersects(*c))
    }

    /// Block structure in the minimap's window: headings as thick bars,
    /// paragraphs as rows of ink, code blocks as tinted boxes.
    fn paint_minimap(
        &self,
        painter: &egui::Painter,
        ui: &Ui,
        doc: &Document,
        parse: &ParseOutput,
        rect: Rect,
        viewport: f32,
    ) {
        let heights = &self.lines.heights;
        if heights.is_empty() {
            return;
        }
        let m = self.lines.minimap(rect, viewport);
        m.paint_background(painter, self.theme.mini_background);
        let (top, bottom) = m.window();
        let row = f64::from(self.text.row_height());
        let start = heights.line_at(top).line;
        for leaf in parse.blocks.leaves_from(doc.line_to_byte(start)) {
            let r = &leaf.block.range;
            let first = doc.byte_to_line(r.start);
            let last = doc.byte_to_line(r.end.saturating_sub(1).max(r.start));
            let y = heights.offset_of(first);
            if y > bottom {
                break;
            }
            let h = heights.offset_of(last + 1) - y;
            let indent = (leaf
                .containers
                .iter()
                .filter(|c| !matches!(c.kind, BlockKind::List { .. }))
                .count() as f32
                * 0.07)
                .min(0.5);
            match leaf.block.kind {
                BlockKind::Heading(level) => {
                    let width = 1.0 - 0.12 * f32::from(level.saturating_sub(1));
                    m.bar(
                        painter,
                        y + h * 0.35,
                        h * 0.45,
                        (indent, width.max(indent + 0.2)),
                        self.theme.mini_heading,
                    );
                }
                BlockKind::CodeBlock { .. } | BlockKind::HtmlBlock => {
                    m.bar(
                        painter,
                        y,
                        h,
                        (indent, 1.0),
                        self.theme.mini_code_background,
                    );
                    let rows = (h / row).round().max(1.0) as usize;
                    for i in 0..rows {
                        let w = 0.35 + 0.4 * ink(first + i);
                        m.bar(
                            painter,
                            y + (i as f64 + 0.3) * row,
                            row * 0.4,
                            (indent + 0.04, indent + w),
                            self.theme.mini_code,
                        );
                    }
                }
                BlockKind::ThematicBreak => {
                    m.bar(
                        painter,
                        y + h / 2.0,
                        8.0,
                        (indent, 1.0),
                        self.theme.mini_text,
                    );
                }
                _ => {
                    let rows = (h / row).round().max(1.0) as usize;
                    for i in 0..rows {
                        let w = if i + 1 == rows && rows > 1 {
                            0.3 + 0.4 * ink(first + i)
                        } else {
                            0.85 + 0.15 * ink(first + i)
                        };
                        m.bar(
                            painter,
                            y + (i as f64 + 0.3) * row,
                            row * 0.4,
                            (indent, (indent + w).min(1.0)),
                            self.theme.mini_text,
                        );
                    }
                }
            }
        }
        let fill = if ui.rect_contains_pointer(rect) {
            self.theme.mini_viewport_hover
        } else {
            self.theme.mini_viewport
        };
        m.paint_viewport(painter, fill, self.theme.mini_viewport_edge);
    }

    /// Draws `body`'s segments at `origin`, with selection highlights and
    /// strikethrough lines. `hidden` draws only the selection.
    fn draw_body(
        &mut self,
        painter: &egui::Painter,
        meshes: &mut GlyphMeshes,
        body: &Body,
        origin: Pos2,
        selection: &Range<usize>,
        hidden: bool,
    ) {
        let range = body.range.clone();
        let sel = selection.start.max(range.start)..selection.end.min(range.end);
        let (s_seg, s_d) = body.layout.display_pos(sel.start);
        let (e_seg, e_d) = body.layout.display_pos(sel.end);
        for (i, seg) in body.layout.segments.iter().enumerate() {
            let o = pos2(origin.x, origin.y + body.seg_tops[i]);
            if !selection.is_empty() && sel.start < sel.end && (s_seg..=e_seg).contains(&i) {
                let start = if i == s_seg { s_d } else { 0 };
                let end = if i == e_seg { e_d } else { seg.text.len() };
                let mut rects = Vec::new();
                body.geometry[i].selection_rects(
                    start..end,
                    i < e_seg || selection.end > range.end,
                    NEWLINE_WIDTH,
                    &mut rects,
                );
                for r in rects {
                    painter.rect_filled(r.translate(o.to_vec2()), 0.0, self.theme.selection);
                }
            }
            if hidden || (body.layout.style == LeafStyle::Rule && seg.text.is_empty()) {
                continue;
            }
            let skip: Vec<Range<usize>> = seg.maths.iter().map(|m| m.display.clone()).collect();
            self.text.draw_rich(
                meshes,
                RichLine {
                    text: &seg.text,
                    runs: &seg.runs,
                    wrap_width: Some(body.wrap),
                },
                o,
                self.theme.text,
                &seg.colors,
                &skip,
            );
            if let Some(ctx) = self.ctx.clone() {
                let paint = self.paint_for(body.wrap);
                for math in &seg.maths {
                    let Some((texture, size)) =
                        self.math
                            .texture(&ctx, &math.tex, math.display_style, &paint)
                    else {
                        continue;
                    };
                    let Some(rect) =
                        crate::math::formula_rect(&body.geometry[i], math.display.clone(), size)
                    else {
                        continue;
                    };
                    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                    painter.image(
                        texture.id(),
                        rect.translate(o.to_vec2()),
                        uv,
                        Color32::WHITE,
                    );
                }
            }
            for strike in &seg.strikes {
                let mut rects = Vec::new();
                body.geometry[i].selection_rects(strike.clone(), false, 0.0, &mut rects);
                for r in rects {
                    let r = r.translate(o.to_vec2());
                    painter.hline(
                        r.x_range(),
                        r.top() + r.height() * 0.55,
                        Stroke::new(1.3, self.theme.struck),
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_leaf(
        &mut self,
        painter: &egui::Painter,
        meshes: &mut GlyphMeshes,
        doc: &Document,
        parse: &ParseOutput,
        p: &Placed,
        frame: Frame,
        top: f32,
        selection: &Range<usize>,
    ) {
        let right = frame.left + frame.width;
        // Container decorations: quote bars and the item's bullet or number.
        let mut x = frame.left;
        for c in &p.leaf.containers {
            match c.kind {
                BlockKind::BlockQuote => {
                    let bar = Rect::from_min_max(pos2(x + 4.0, top), pos2(x + 7.0, top + p.height));
                    painter.rect_filled(bar, 1.0, self.theme.quote_bar);
                }
                BlockKind::Item if doc.byte_to_line(c.range.start) == p.first_line => {
                    let y = top + p.body.seg_tops.first().copied().unwrap_or(0.0);
                    let row = p
                        .body
                        .geometry
                        .first()
                        .and_then(|g| g.rows.first())
                        .map_or(self.text.row_height(), |r| r.height);
                    match task_marker(parse, c.range.start, p.leaf.block.range.start) {
                        Some((at, checked)) => {
                            let size = (row * 0.62).round();
                            let b = Rect::from_min_size(
                                pos2(x + 2.0, y + ((row - size) / 2.0).round()),
                                vec2(size, size),
                            );
                            if checked {
                                painter.rect_filled(b, 3.0, self.theme.checkbox_done);
                                let tick = [
                                    pos2(b.left() + size * 0.22, b.top() + size * 0.52),
                                    pos2(b.left() + size * 0.42, b.top() + size * 0.72),
                                    pos2(b.left() + size * 0.78, b.top() + size * 0.3),
                                ];
                                painter
                                    .line(tick.to_vec(), Stroke::new(2.0, self.theme.background));
                            } else {
                                painter.rect_stroke(
                                    b,
                                    3.0,
                                    Stroke::new(1.5, self.theme.list_marker),
                                    StrokeKind::Inside,
                                );
                            }
                            self.checkboxes.push((b.expand(3.0), at, checked));
                        }
                        None => {
                            let marker = list_marker(doc, parse, c, &p.leaf.containers);
                            self.text.draw_line(
                                meshes,
                                &marker,
                                pos2(x + 4.0, y),
                                self.theme.list_marker,
                            );
                        }
                    }
                }
                BlockKind::FootnoteDefinition => {
                    if let Some(label) = footnote_label(doc, parse, c, &p.leaf.block) {
                        let y = top + p.body.seg_tops.first().copied().unwrap_or(0.0);
                        let marker = self.fit_marker(&label, FOOTNOTE_INDENT - 6.0);
                        self.text.draw_line(
                            meshes,
                            &marker,
                            pos2(x + 2.0, y),
                            self.theme.list_marker,
                        );
                    }
                }
                _ => {}
            }
            x += container_indent(c);
        }
        match p.body.layout.style {
            LeafStyle::Code | LeafStyle::Html => {
                let bg = Rect::from_min_max(pos2(x, top + 2.0), pos2(right, top + p.height - 2.0));
                painter.rect_filled(bg, 4.0, self.theme.code_background);
            }
            LeafStyle::Rule if p.body.layout.segments.iter().all(|s| s.text.is_empty()) => {
                let y = top + p.height / 2.0;
                painter.line_segment(
                    [pos2(x, y), pos2(right, y)],
                    Stroke::new(1.5, self.theme.rule),
                );
            }
            _ => {}
        }
        let text_left = frame.left + p.indent;
        if let Some(grid) = &p.table {
            let origin = pos2(text_left, top + TABLE_PAD);
            let r = Rect::from_min_size(origin, vec2(grid.width(), grid.height()));
            if grid.has_head && !grid.row_h.is_empty() {
                let head = Rect::from_min_size(origin, vec2(grid.width(), grid.row_h[0]));
                painter.rect_filled(head, 0.0, self.theme.code_background);
            }
            let line = Stroke::new(1.0, self.theme.quote_bar);
            for y in grid.row_y.iter().skip(1) {
                painter.hline(r.x_range(), origin.y + y, line);
            }
            for x in grid.col_x.iter().skip(1) {
                painter.vline(origin.x + x, r.y_range(), line);
            }
            painter.rect_stroke(r, 3.0, line, StrokeKind::Inside);
            for cell in &grid.cells {
                self.draw_body(
                    painter,
                    meshes,
                    &cell.body,
                    origin + cell.origin,
                    selection,
                    false,
                );
            }
        } else {
            let hidden = p.text_hidden;
            self.draw_body(
                painter,
                meshes,
                &p.body,
                pos2(text_left, top),
                selection,
                hidden,
            );
        }
        for image in &p.images {
            let r = Rect::from_min_size(pos2(text_left, top + image.top), image.size);
            let label = match &image.slot {
                ImageSlot::Ready { texture, .. } => {
                    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                    painter.image(texture.id(), r, uv, Color32::WHITE);
                    continue;
                }
                ImageSlot::Loading => "Loading image…".to_owned(),
                ImageSlot::Failed(e) => format!("Can't show {}: {e}", image.dest),
                ImageSlot::Remote => format!("Remote image not loaded: {}", image.dest),
            };
            painter.rect_filled(r, 4.0, self.theme.code_background);
            painter.rect_stroke(
                r,
                4.0,
                Stroke::new(1.0, self.theme.quote_bar),
                StrokeKind::Inside,
            );
            let label_pos = pos2(r.left() + 10.0, r.center().y - self.text.row_height() / 2.0);
            self.text
                .draw_line(meshes, &label, label_pos, self.theme.markup);
        }
    }
}

/// The caret's place in a table (see `LiveView::table_at_caret`).
struct TableAt {
    rows: Vec<(inkmark_parse::Block, Vec<inkmark_parse::Block>)>,
    row: usize,
    col: usize,
}

/// Where typing in `cell` should start: its first non-space byte, or just
/// inside the padding of an empty cell.
fn cell_text_start(doc: &Document, cell: &inkmark_parse::Block) -> usize {
    let text = doc.slice(cell.range.clone());
    let lead = text.len() - text.trim_start().len();
    if lead == text.len() {
        cell.range.start + text.len().min(1)
    } else {
        cell.range.start + lead
    }
}

/// Column alignments from a table's delimiter row (`:--`, `:-:`, `--:`),
/// the line after the header.
fn table_alignments(
    doc: &Document,
    rows: &[(inkmark_parse::Block, Vec<inkmark_parse::Block>)],
    columns: usize,
) -> Vec<Align> {
    let mut aligns = vec![Align::Left; columns];
    let Some((head, _)) = rows.first() else {
        return aligns;
    };
    let line = doc.byte_to_line(head.range.end.saturating_sub(1).max(head.range.start)) + 1;
    if line >= doc.line_count() {
        return aligns;
    }
    let text = doc.slice(doc.line_range(line)).into_owned();
    let trimmed = text.trim().trim_start_matches('|').trim_end_matches('|');
    for (i, part) in trimmed.split('|').take(columns).enumerate() {
        let part = part.trim();
        aligns[i] = match (part.starts_with(':'), part.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        };
    }
    aligns
}

/// The GFM task marker of the item starting at `item_start`, if any:
/// (offset of `[`, checked). It sits between the bullet and the text.
fn task_marker(parse: &ParseOutput, item_start: usize, text_start: usize) -> Option<(usize, bool)> {
    parse
        .map
        .spans_in(item_start..text_start.max(item_start + 1))
        .into_iter()
        .find_map(|s| match s.kind {
            SpanKind::Syntax(Syntax::TaskMarker(checked)) => Some((s.range.start, checked)),
            _ => None,
        })
}

/// A stable pseudo-random 0..1 per row, so paragraph ink looks ragged like
/// text without changing from frame to frame.
fn ink(seed: usize) -> f32 {
    let mut x = seed as u64 ^ 0x9e37_79b9_7f4a_7c15;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x % 1000) as f32 / 1000.0
}

fn container_indent(c: &inkmark_parse::Block) -> f32 {
    match c.kind {
        BlockKind::BlockQuote => QUOTE_INDENT,
        BlockKind::Item => ITEM_INDENT,
        BlockKind::FootnoteDefinition => FOOTNOTE_INDENT,
        _ => 0.0,
    }
}

fn reveal_at(doc: &Document, caret: usize) -> Reveal {
    Reveal {
        line: doc.line_range(doc.byte_to_line(caret)),
        caret,
    }
}

/// The leaf block shown on source `line`, if any.
fn leaf_at_line(doc: &Document, parse: &ParseOutput, line: usize) -> Option<Leaf> {
    let leaf = parse.blocks.leaves_from(doc.line_to_byte(line)).next()?;
    (doc.byte_to_line(leaf.block.range.start) <= line).then_some(leaf)
}

/// What a step over a long-line notice is for (see `LiveView::step`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Move,
    Select,
    Delete,
}

/// The source a long-line notice stands for (see
/// `long_line::notice_source`), when `at` is in it or on an edge: the
/// block's, or in a table the cell's.
fn notice_at(doc: &Document, parse: &ParseOutput, at: usize) -> Option<Range<usize>> {
    let leaf = leaf_at_line(doc, parse, doc.byte_to_line(at))?;
    let block = if matches!(leaf.block.kind, BlockKind::Table { .. }) {
        parse
            .blocks
            .table_rows(&leaf.block)
            .into_iter()
            .flat_map(|(_, cells)| cells)
            .find(|c| c.range.start <= at && at <= c.range.end)?
    } else {
        leaf.block
    };
    if !crate::long_line::shows_notice(doc, &block) {
        return None;
    }
    let source = crate::long_line::notice_source(doc, block.range);
    (source.start <= at && at <= source.end).then_some(source)
}

/// First and last source line of the leaf on `line`.
fn leaf_lines(doc: &Document, parse: &ParseOutput, line: usize) -> Option<(usize, usize)> {
    let leaf = leaf_at_line(doc, parse, line)?;
    let r = leaf.block.range;
    Some((
        doc.byte_to_line(r.start),
        doc.byte_to_line(r.end.saturating_sub(1).max(r.start)),
    ))
}

/// A footnote definition's label, as written, when `leaf` is its first leaf
/// (the margin marker goes beside that one only). Read from the `[^label]:`
/// that opens the definition; the first block may itself be a list or a
/// quote, and a label may contain escaped brackets (`[^a\]b]`).
fn footnote_label(
    doc: &Document,
    parse: &ParseOutput,
    def: &inkmark_parse::Block,
    leaf: &inkmark_parse::Block,
) -> Option<String> {
    let first = parse.blocks.leaves_from(def.range.start).next()?;
    if first.block.range.start != leaf.range.start {
        return None;
    }
    let line_end = doc
        .line_range(doc.byte_to_line(def.range.start))
        .end
        .min(def.range.end);
    let head = doc.slice(def.range.start..line_end);
    inkmark_parse::definition_label(&head).map(str::to_owned)
}

/// "•" (by nesting depth) for bullet items. Ordered items count up from
/// the list's start number, as CommonMark renders them ("1. 1. 1." shows
/// 1, 2, 3), keeping the source's "." or ")".
fn list_marker(
    doc: &Document,
    parse: &ParseOutput,
    item: &inkmark_parse::Block,
    containers: &[inkmark_parse::Block],
) -> String {
    let list = containers
        .iter()
        .rev()
        .find(|c| matches!(c.kind, BlockKind::List { .. }) && c.range.start <= item.range.start);
    if let Some(
        list @ inkmark_parse::Block {
            kind:
                BlockKind::List {
                    ordered: true,
                    start,
                },
            ..
        },
    ) = list
    {
        let line_end = doc.line_range(doc.byte_to_line(item.range.start)).end;
        let source = doc.slice(item.range.start..line_end);
        let delimiter = source
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .chars()
            .next()
            .filter(|c| *c == ')')
            .unwrap_or('.');
        let n = start + parse.blocks.item_index(list, item.range.start) as u64;
        return format!("{n}{delimiter}");
    }
    let depth = containers
        .iter()
        .filter(|c| matches!(c.kind, BlockKind::List { .. }))
        .count();
    ["•", "◦", "▪"][depth.saturating_sub(1) % 3].to_owned()
}

#[cfg(test)]
mod tests {
    use inkmark_parse::{MarkdownParser, PulldownParser};

    use super::*;

    fn markers(src: &str) -> Vec<String> {
        let doc = Document::from_text(src);
        let parse = PulldownParser.parse(src);
        parse
            .blocks
            .leaves_from(0)
            .map(|leaf| {
                let item = leaf.containers.last().expect("in an item").clone();
                list_marker(&doc, &parse, &item, &leaf.containers)
            })
            .collect()
    }

    /// The margin label drawn beside each leaf of `src`, if any.
    fn footnote_labels(src: &str) -> Vec<Option<String>> {
        use inkmark_parse::GfmParser;

        let doc = Document::from_text(src);
        let parse = GfmParser.parse(src);
        parse
            .blocks
            .leaves_from(0)
            .map(|leaf| {
                let def = leaf
                    .containers
                    .iter()
                    .find(|c| c.kind == BlockKind::FootnoteDefinition)?;
                footnote_label(&doc, &parse, def, &leaf.block)
            })
            .collect()
    }

    #[test]
    fn a_footnote_label_is_read_from_the_definitions_start() {
        // Review of #23: a first block that is a list or quote, or a label
        // with an escaped bracket, used to lose its margin label.
        let one = || Some("1".to_owned());
        assert_eq!(footnote_labels("[^1]: Note\n"), vec![one()]);
        assert_eq!(footnote_labels("[^1]: - item\n"), vec![one()]);
        assert_eq!(footnote_labels("[^1]: 1. item\n"), vec![one()]);
        assert_eq!(footnote_labels("[^1]: > quote\n"), vec![one()]);
        assert_eq!(
            footnote_labels("[^foo\\]bar]: note\n"),
            vec![Some("foo\\]bar".to_owned())]
        );
        // Beside the first leaf only.
        assert_eq!(
            footnote_labels("[^1]: First\n\n    Second\n"),
            vec![one(), None]
        );
        assert_eq!(footnote_labels("[^1]: - a\n    - b\n"), vec![one(), None]);
    }

    #[test]
    fn ordered_items_count_up_from_the_start_number() {
        assert_eq!(markers("7. a\n7. b\n7. c\n"), vec!["7.", "8.", "9."]);
        assert_eq!(markers("1) a\n1) b\n"), vec!["1)", "2)"]);
        assert_eq!(markers("- a\n  - b\n"), vec!["•", "◦"]);
    }

    #[test]
    fn a_table_wider_than_the_pane_shares_the_width_out() {
        use inkmark_parse::GfmParser;

        let ctx = egui::Context::default();
        let mut view = LiveView::with_fonts(inkmark_text::Fonts::shared(&ctx), Id::new("t"));
        let width = 360.0;
        view.text.begin_frame(
            TextConfig {
                monospace: false,
                font_size: view.font_size,
                line_height: view.line_height,
                wrap_width: Some(width),
            },
            1.0,
        );
        let long = "a long cell with many words in it ".repeat(4);
        let src = format!("| one | {long} | three |\n|---|---|---|\n| x | y | z |\n");
        let doc = Document::from_text(&src);
        let parse = GfmParser.parse(&src);
        let leaf = parse.blocks.leaves_from(0).next().unwrap();
        let placed = view.place(&doc, &parse, leaf, width);
        let grid = placed.table.expect("a table");
        assert!(
            grid.width() <= width + 1.0,
            "fits: {} in {width}",
            grid.width()
        );
        // The long column wraps instead of squeezing the short ones to
        // nothing: each keeps room for its longest word.
        assert!(
            grid.col_w[1] > grid.col_w[0] && grid.col_w[1] > grid.col_w[2],
            "{:?}",
            grid.col_w
        );
        assert!(grid.col_w.iter().all(|w| *w > 20.0), "{:?}", grid.col_w);
        assert!(grid.row_h[0] > grid.row_h[1], "the long row wraps taller");
    }

    #[test]
    fn a_formula_is_drawn_and_the_source_stays() {
        use inkmark_parse::GfmParser;

        let ctx = egui::Context::default();
        let mut view = LiveView::with_fonts(inkmark_text::Fonts::shared(&ctx), Id::new("math"));
        let width = 600.0;
        view.text.begin_frame(
            TextConfig {
                monospace: false,
                font_size: view.font_size,
                line_height: view.line_height,
                wrap_width: Some(width),
            },
            1.0,
        );
        let src = "Energy $E=mc^2$ today.\n\n$$\\frac{1}{2}$$\n\n$\\frac{$\n";
        let doc = Document::from_text(src);
        let parse = GfmParser.parse(src);
        let inline = view.place(
            &doc,
            &parse,
            parse.blocks.leaves_from(0).next().unwrap(),
            width,
        );
        let text = inline.body.layout.segments[0].text.as_str();
        assert!(!text.contains("E=mc"), "{text}");
        assert!(text.contains('M'), "{text}");
        assert!(
            text.starts_with("Energy ") && text.ends_with(" today."),
            "{text}"
        );
        assert_eq!(inline.body.layout.segments[0].maths[0].tex, "E=mc^2");
        assert_eq!(doc.slice(0..doc.len()).as_ref(), src);
        // The code pane can hold the caret inside the formula. The live pane
        // then shows those bytes, focused or not, so the caret stays on them.
        view.selection = Selection::caret(src.find("mc").unwrap());
        let shown = view.place(
            &doc,
            &parse,
            parse.blocks.leaves_from(0).next().unwrap(),
            width,
        );
        let shown_text: String = shown
            .body
            .layout
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(shown_text.contains("$E=mc^2$"), "{shown_text}");
        view.selection = Selection::caret(0);

        let display = view.place(
            &doc,
            &parse,
            parse
                .blocks
                .leaves_from(src.find("frac").unwrap())
                .next()
                .unwrap(),
            width,
        );
        assert!(
            display
                .body
                .layout
                .segments
                .iter()
                .any(|s| { s.maths.iter().any(|m| m.display_style) && !s.text.contains("frac") })
        );
        let row = view.text.row_height();
        assert_eq!(display.body.layout.segments.len(), 1);
        assert_eq!(display.body.geometry.len(), 1);
        assert_eq!(display.body.geometry[0].rows.len(), 1);
        let drawn = display.body.geometry[0].height();
        assert!(drawn > row, "display row {drawn} vs text {row}");
        let end = display.body.layout.segments[0].maths[0].source.end;
        assert_eq!(display.body.layout.display_pos(end).0, 0);

        let bad = view.place(
            &doc,
            &parse,
            parse
                .blocks
                .leaves_from(src.rfind('$').unwrap())
                .next()
                .unwrap(),
            width,
        );
        let bad_text: String = bad
            .body
            .layout
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert!(bad_text.contains("frac"), "{bad_text}");
        assert!(bad.body.layout.segments.iter().all(|s| s.maths.is_empty()));

        let table_src = "| $E=mc^2$ | a |\n|---|---|\n| b | c |\n";
        let doc = Document::from_text(table_src);
        let parse = GfmParser.parse(table_src);
        let placed = view.place(
            &doc,
            &parse,
            parse.blocks.leaves_from(0).next().unwrap(),
            width,
        );
        let grid = placed.table.expect("table");
        assert!(grid.col_w[0] > grid.col_w[1], "{:?}", grid.col_w);
        let cell = &grid.cells[0].body.layout.segments[0].text;
        assert!(cell.contains('M') && !cell.contains("mc"), "{cell}");
    }

    #[test]
    fn a_wide_formula_stays_on_one_line() {
        use inkmark_parse::GfmParser;

        let ctx = egui::Context::default();
        let mut view = LiveView::with_fonts(inkmark_text::Fonts::shared(&ctx), Id::new("wide"));
        let width = 220.0;
        view.text.begin_frame(
            TextConfig {
                monospace: false,
                font_size: view.font_size,
                line_height: view.line_height,
                wrap_width: Some(width),
            },
            1.0,
        );
        let src = format!("${}$\n", "x+".repeat(39) + "x");
        view.selection = Selection::caret(src.len());
        let doc = Document::from_text(&src);
        let parse = GfmParser.parse(&src);
        let placed = view.place(
            &doc,
            &parse,
            parse.blocks.leaves_from(0).next().unwrap(),
            width,
        );
        assert!(placed.body.geometry.iter().all(|g| g.rows.len() == 1));
        assert!(
            placed.body.width() <= width + 1.0,
            "{}",
            placed.body.width()
        );
        assert_eq!(doc.slice(0..doc.len()).as_ref(), src);
    }
}
