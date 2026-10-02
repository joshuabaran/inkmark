//! The rendered pane: leaf blocks laid out as rich text, Markdown syntax
//! hidden except on the caret's line. Navigable but read-only for now; edits
//! arrive with source-patching in M4.

use std::ops::Range;

use egui::output::IMEOutput;
use egui::{
    Color32, CursorIcon, Event, EventFilter, IMEPurpose, Id, ImeEvent, Key, Modifiers, Pos2, Rect,
    Response, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};
use inkmark_buffer::{Bias, Document, Edit, EditKind, Selection};
use inkmark_parse::{
    BlockKind, Leaf, ParseOutput, ParseState, SpanKind, Style, Syntax, inline_images,
};
use inkmark_text::{
    GlyphMeshes, LineGeometry, RichLine, ScrollAnchor, SharedFonts, TextConfig, TextRenderer,
};

use crate::commands::{self, EditPlan, EnterContext};
use crate::images::{ImageCache, ImageSlot};
use crate::lines::{LineIndex, SCROLLBAR_WIDTH, ScrollPos, Synced};
use crate::live_layout::{self, LeafLayout, LeafStyle, Reveal};
use crate::motion;
use crate::theme::{
    self, BACKGROUND, CARET, CODE_BACKGROUND, LIST_MARKER, MARKUP, QUOTE_BAR, RULE, SELECTION, TEXT,
};

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
    fn caret_rect(&self, at: usize) -> Rect {
        let (seg, d) = self.layout.display_pos(at);
        self.geometry[seg]
            .caret_rect(d, CARET_WIDTH)
            .translate(vec2(0.0, self.seg_tops[seg]))
    }

    /// Source offset at `local` (from the body's top-left).
    fn hit(&self, local: Vec2) -> usize {
        let seg = self.segment_at(local.y);
        let d = self.geometry[seg].hit(vec2(local.x, local.y - self.seg_tops[seg]));
        self.layout.source_pos(seg, d)
    }

    /// Start or end of the visual row holding `at`.
    fn row_edge(&self, at: usize, end: bool) -> usize {
        let (seg, d) = self.layout.display_pos(at);
        let g = &self.geometry[seg];
        let row = g.row_of(d);
        let d = if end {
            g.hit_row(row, f32::INFINITY)
        } else {
            g.rows[row].start
        };
        self.layout.source_pos(seg, d)
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
    fn caret_rect(&self, at: usize) -> Rect {
        match self.table.as_ref().and_then(|g| g.cell_for(at)) {
            Some(cell) => cell
                .body
                .caret_rect(at)
                .translate(cell.origin + vec2(0.0, TABLE_PAD)),
            None => self.body.caret_rect(at),
        }
    }

    /// Source offset at `local`, from the leaf's text origin.
    fn hit(&self, local: Vec2) -> usize {
        let local_in_table = local - vec2(0.0, TABLE_PAD);
        match self.table.as_ref().and_then(|g| g.cell_at(local_in_table)) {
            Some(cell) => cell.body.hit(local_in_table - cell.origin),
            None => self.body.hit(local),
        }
    }

    fn row_edge(&self, at: usize, end: bool) -> usize {
        match self.table.as_ref().and_then(|g| g.cell_for(at)) {
            Some(cell) => cell.body.row_edge(at, end),
            None => self.body.row_edge(at, end),
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
    /// The selection was set from outside in current offsets: don't map it
    /// through edits on the next sync.
    selection_current: bool,
    /// A scroll position set from outside, applied after the next sync.
    pending_scroll: Option<ScrollPos>,
    /// Task checkboxes drawn last frame: hit area, source offset of the
    /// `[ ]` marker, checked.
    checkboxes: Vec<(Rect, usize, bool)>,
    /// Created on the first frame, when an egui context is at hand.
    images: Option<ImageCache>,
    pub font_size: f32,
    pub line_height: f32,
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
            selection_current: false,
            pending_scroll: None,
            checkboxes: Vec::new(),
            images: None,
            font_size: 16.0,
            line_height: 26.0,
        }
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
        self.lines.heights.set_measured(p.first_line, p.height);
        for l in p.first_line + 1..=p.last_line {
            self.lines.heights.set_measured(l, 0.0);
        }
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
        let rect = ui.available_rect_before_wrap();
        ui.advance_cursor_after_rect(rect);
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
        let frame = Frame {
            rect,
            left: rect.left() + PADDING,
            width: (text_right - rect.left() - 2.0 * PADDING).max(80.0),
        };
        let text_rect = Rect::from_min_max(rect.min, pos2(text_right, rect.bottom()));
        let response = ui.interact(text_rect, self.id, Sense::click_and_drag());
        if response.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::Text);
        }
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                self.id,
                EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: false,
                },
            )
        });
        self.focused = response.has_focus();

        let config = TextConfig {
            monospace: false,
            font_size: self.font_size,
            line_height: self.line_height,
            wrap_width: Some(frame.width),
        };
        if self.text.begin_frame(config, ui.ctx().pixels_per_point()) {
            self.lines.invalidate();
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BACKGROUND);
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
        let mut minimap_hovered = false;
        if let Some(r) = minimap {
            let (scrolled, hovered) = self.lines.minimap_input(ui, self.id, r, rect.height());
            self.scrolled |= scrolled;
            minimap_hovered = hovered;
        }
        if self
            .lines
            .scroll_input(ui, self.id, response.hovered() || minimap_hovered, bar)
        {
            self.scrolled = true;
        }
        if self.reveal_caret > 0 {
            self.scroll_caret_into_view(ui, doc, parse, frame);
        }
        let caret = self.paint(&painter, doc, parse, frame);
        self.lines.paint_scrollbar(&painter, bar);
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
        if let Synced::Changed(changes) = self.lines.sync(doc, |chars| text.estimate_height(chars))
            && map_selection
        {
            for c in &changes {
                self.selection.anchor = c.map(self.selection.anchor, Bias::Left);
                self.selection.head = c.map(self.selection.head, Bias::Left);
            }
        }
        self.selection.anchor = self.selection.anchor.min(doc.len());
        self.selection.head = self.selection.head.min(doc.len());
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

    fn place(&mut self, doc: &Document, parse: &ParseOutput, leaf: Leaf, width: f32) -> Placed {
        let (map, link_defs) = (&parse.map, &parse.link_defs);
        let layout = live_layout::build(doc, map, &leaf, self.reveal(doc));
        let containers: f32 = leaf.containers.iter().map(container_indent).sum();
        let row = self.text.row_height();
        let (pad_top, pad_bottom, inner) = match layout.style {
            LeafStyle::Heading(level) => {
                (row * if level <= 2 { 0.7 } else { 0.45 }, row * 0.25, 0.0)
            }
            LeafStyle::Code | LeafStyle::Html => (CODE_PAD, CODE_PAD, CODE_PAD),
            LeafStyle::Paragraph | LeafStyle::Rule => (0.0, 0.0, 0.0),
        };
        let range = leaf.block.range.clone();
        let first_line = doc.byte_to_line(range.start);
        let last_line = doc.byte_to_line(range.end.saturating_sub(1).max(range.start));

        let spans = map.spans_in(range.clone());
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
        let mut y = pad_top;
        let mut seg_tops = Vec::with_capacity(layout.segments.len());
        let mut geometry = Vec::with_capacity(layout.segments.len());
        for seg in &layout.segments {
            let g = self.text.rich_geometry(RichLine {
                text: &seg.text,
                runs: &seg.runs,
                wrap_width: Some(wrap),
            });
            seg_tops.push(y);
            if !text_hidden {
                y += g.height();
            }
            geometry.push(g);
        }

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
        let layouts: Vec<Vec<(inkmark_parse::Block, LeafLayout)>> = rows
            .iter()
            .map(|(_, cells)| {
                cells
                    .iter()
                    .take(columns)
                    .map(|cell| {
                        let leaf = Leaf {
                            block: cell.clone(),
                            containers: Vec::new(),
                        };
                        (
                            cell.clone(),
                            live_layout::build(doc, &parse.map, &leaf, reveal.clone()),
                        )
                    })
                    .collect()
            })
            .collect();
        let measure = |text: &mut TextRenderer, layout: &LeafLayout, wrap: f32| -> Body {
            let mut y = 0.0;
            let mut seg_tops = Vec::new();
            let mut geometry = Vec::new();
            for seg in &layout.segments {
                let g = text.rich_geometry(RichLine {
                    text: &seg.text,
                    runs: &seg.runs,
                    wrap_width: Some(wrap),
                });
                seg_tops.push(y);
                y += g.height();
                geometry.push(g);
            }
            Body {
                layout: layout.clone(),
                seg_tops,
                geometry,
                range: 0..0,
                wrap,
            }
        };
        // Each column's widest cell unwrapped (max) and its longest word
        // (min). Columns get their max if everything fits; otherwise the
        // space above the minimums is shared in proportion, so short
        // columns aren't broken mid-word to make room for long ones.
        let mut max_w = vec![MIN_COLUMN; columns];
        let mut min_w = vec![MIN_COLUMN; columns];
        for row in &layouts {
            for (c, (_, layout)) in row.iter().enumerate() {
                // A little slack: wrapping at exactly the measured width can
                // still break the line after pixel rounding.
                let natural = measure(&mut self.text, layout, 100_000.0).width() + 2.0;
                max_w[c] = max_w[c].max(natural + 2.0 * CELL_PAD_X);
                let word = layout
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
        for row in &layouts {
            let mut h = row_min;
            let start = cells.len();
            for (c, (block, layout)) in row.iter().enumerate() {
                let inner = col_w[c] - 2.0 * CELL_PAD_X;
                let mut body = measure(&mut self.text, layout, inner.max(8.0));
                body.range = block.range.clone();
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
                let r = p.caret_rect(head);
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
                p.hit(vec2(x - p.indent, local))
            }
            None => doc.line_to_byte(at.line),
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
        let layout = live_layout::build(doc, &parse.map, &leaf, Some(reveal_at(doc, offset)));
        let (seg, d) = layout.display_pos(offset);
        layout.source_pos(seg, d) == offset
    }

    /// One step left or right (by grapheme or word), skipping hidden bytes.
    fn step(&self, doc: &Document, parse: &ParseOutput, forward: bool, word: bool) -> usize {
        let mut at = self.selection.head;
        for _ in 0..256 {
            let next = match (forward, word) {
                (true, false) => motion::next_grapheme(doc, at),
                (true, true) => motion::next_word(doc, at),
                (false, false) => motion::prev_grapheme(doc, at),
                (false, true) => motion::prev_word(doc, at),
            };
            if next == at {
                break;
            }
            at = next;
            if self.is_visible(doc, parse, at) {
                break;
            }
        }
        at
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
        self.move_to(target, extend);
        self.preferred_x = Some(x);
    }

    fn row_edge(&mut self, doc: &Document, parse: &ParseOutput, frame: Frame, end: bool) -> usize {
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        let Some(p) = self.place_line(doc, parse, line, frame.width) else {
            return doc.line_to_byte(line);
        };
        p.row_edge(head, end)
    }

    // ---- input ---------------------------------------------------------------

    /// Keyboard, clipboard and IME input. Each edit is re-parsed before
    /// the next event, so navigation never reads a stale map.
    fn handle_events(&mut self, ui: &Ui, doc: &mut Document, state: &mut ParseState, frame: Frame) {
        let events = ui.input(|i| i.events.clone());
        for event in events {
            let parse = state.output();
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
        let (cmd, shift, alt) = (modifiers.command, modifiers.shift, modifiers.alt);
        let sel = self.selection;
        let range = sel.range();
        if !cmd
            && !alt
            && matches!(key, Key::Tab | Key::Enter)
            && let Some(edited) = self.table_key(doc, parse, key, shift)
        {
            return edited;
        }
        match key {
            // Editing.
            Key::Backspace | Key::Delete if !range.is_empty() => {
                return self.delete(doc, range, EditKind::Deleting);
            }
            Key::Backspace => {
                if !cmd && let Some(plan) = commands::smart_backspace(doc, sel) {
                    return self.apply_plan(doc, plan);
                }
                let start = self.step(doc, parse, false, cmd);
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
                let end = self.step(doc, parse, true, cmd);
                if let Some(text) = self.cell_text_range(doc, parse)
                    && end > text.end
                {
                    return false;
                }
                return self.delete(doc, sel.head..end, EditKind::Deleting);
            }
            Key::Enter if cmd => return self.apply_plan(doc, commands::toggle_task(doc, sel)),
            Key::X if cmd && shift => {
                return self.apply_plan(doc, commands::toggle_wrap(doc, sel, "~~", &["~"]));
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
            Key::B if cmd => {
                return self.apply_plan(doc, commands::toggle_wrap(doc, sel, "**", &["__"]));
            }
            Key::I if cmd => {
                return self.apply_plan(doc, commands::toggle_wrap(doc, sel, "*", &["_"]));
            }
            Key::Backtick if cmd => {
                return self.apply_plan(doc, commands::toggle_wrap(doc, sel, "`", &[]));
            }
            Key::K if cmd => return self.apply_plan(doc, commands::insert_link(doc, sel)),
            _ if cmd && alt && commands::heading_level(key).is_some() => {
                let level = commands::heading_level(key).expect("checked");
                return self.apply_plan(doc, commands::set_heading(doc, sel, level));
            }
            Key::Z if cmd => return self.undo(doc, shift),
            Key::Y if cmd => return self.undo(doc, true),
            // Navigation.
            Key::ArrowLeft if !shift && !range.is_empty() => self.move_to(range.start, false),
            Key::ArrowRight if !shift && !range.is_empty() => self.move_to(range.end, false),
            Key::ArrowLeft | Key::ArrowRight => {
                let target = self.step(doc, parse, key == Key::ArrowRight, cmd);
                self.move_to(target, shift);
            }
            Key::ArrowUp => self.move_vertical(doc, parse, frame, -1.0, shift),
            Key::ArrowDown => self.move_vertical(doc, parse, frame, 1.0, shift),
            Key::PageUp => self.move_vertical(doc, parse, frame, -frame.rect.height(), shift),
            Key::PageDown => self.move_vertical(doc, parse, frame, frame.rect.height(), shift),
            Key::Home if cmd => self.move_to(0, shift),
            Key::End if cmd => self.move_to(doc.len(), shift),
            Key::Home => {
                let target = self.row_edge(doc, parse, frame, false);
                self.move_to(target, shift);
            }
            Key::End => {
                let target = self.row_edge(doc, parse, frame, true);
                self.move_to(target, shift);
            }
            Key::A if cmd => {
                self.selection = Selection {
                    anchor: 0,
                    head: doc.len(),
                };
            }
            Key::Escape if !range.is_empty() => self.move_to(sel.head, false),
            _ => {}
        }
        false
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
            self.reveal_caret = 0;
            self.dragging = true;
        } else if self.dragging && down {
            let at = self.offset_at(doc, parse, frame, pos);
            self.selection.head = at;
        }
        if response.double_clicked() {
            let word = motion::word_at(doc, self.selection.head);
            self.selection = Selection {
                anchor: word.start,
                head: word.end,
            };
        }
    }

    fn scroll_caret_into_view(
        &mut self,
        ui: &Ui,
        doc: &Document,
        parse: &ParseOutput,
        frame: Frame,
    ) {
        self.reveal_caret -= 1;
        let (top, _, height) = self.caret_doc(doc, parse, frame);
        let bottom = top + f64::from(height);
        let viewport = f64::from(frame.rect.height());
        let view_top = self.lines.heights.anchor_y(self.lines.anchor);
        if top < view_top {
            self.lines.anchor = self.lines.heights.line_at(top);
        } else if bottom > view_top + viewport {
            self.lines.anchor = self.lines.heights.line_at(bottom - viewport);
        } else {
            self.reveal_caret = 0;
            return;
        }
        self.scrolled = true;
        if self.reveal_caret > 0 {
            ui.ctx().request_repaint();
        }
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
                let text = doc.slice(doc.line_range(line));
                let raw = text.trim_start_matches(|c: char| c.is_whitespace() || c == '>');
                let height = if raw.is_empty() {
                    row * BLANK_LINE
                } else {
                    let h = self.text.line_height(&text);
                    self.text
                        .draw_line(&mut meshes, &text, pos2(frame.left, y), MARKUP);
                    h
                };
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
            if p.first_line < line {
                // Shares a line with the previous leaf: stack below it.
                let prev = self.lines.heights.height(p.first_line);
                self.lines
                    .heights
                    .set_measured(p.first_line, prev + p.height);
            } else {
                self.lines.heights.set_measured(p.first_line, p.height);
            }
            for l in p.first_line + 1..=p.last_line {
                self.lines.heights.set_measured(l, 0.0);
            }
            self.draw_leaf(painter, &mut meshes, doc, parse, &p, frame, y, &selection);
            if (p.first_line..=p.last_line).contains(&caret_line) {
                let r = p.caret_rect(self.selection.head);
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
            painter.rect_filled(bg, 0.0, BACKGROUND);
            self.text.draw_line(&mut meshes, &preedit, c.min, TEXT);
            painter.hline(bg.x_range(), bg.bottom() - 1.0, (1.0, CARET));
        }
        self.text.end_frame(meshes, painter);
        if self.focused
            && let Some(c) = caret
        {
            painter.rect_filled(c, 0.0, CARET);
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
        m.paint_background(painter);
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
                        theme::MINI_HEADING,
                    );
                }
                BlockKind::CodeBlock { .. } | BlockKind::HtmlBlock => {
                    m.bar(painter, y, h, (indent, 1.0), theme::MINI_CODE_BACKGROUND);
                    let rows = (h / row).round().max(1.0) as usize;
                    for i in 0..rows {
                        let w = 0.35 + 0.4 * ink(first + i);
                        m.bar(
                            painter,
                            y + (i as f64 + 0.3) * row,
                            row * 0.4,
                            (indent + 0.04, indent + w),
                            theme::MINI_CODE,
                        );
                    }
                }
                BlockKind::ThematicBreak => {
                    m.bar(painter, y + h / 2.0, 8.0, (indent, 1.0), theme::MINI_TEXT);
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
                            theme::MINI_TEXT,
                        );
                    }
                }
            }
        }
        m.paint_viewport(painter, ui.rect_contains_pointer(rect));
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
                    painter.rect_filled(r.translate(o.to_vec2()), 0.0, SELECTION);
                }
            }
            if hidden || (body.layout.style == LeafStyle::Rule && seg.text.is_empty()) {
                continue;
            }
            self.text.draw_rich(
                meshes,
                RichLine {
                    text: &seg.text,
                    runs: &seg.runs,
                    wrap_width: Some(body.wrap),
                },
                o,
                TEXT,
                &seg.colors,
            );
            for strike in &seg.strikes {
                let mut rects = Vec::new();
                body.geometry[i].selection_rects(strike.clone(), false, 0.0, &mut rects);
                for r in rects {
                    let r = r.translate(o.to_vec2());
                    painter.hline(
                        r.x_range(),
                        r.top() + r.height() * 0.55,
                        Stroke::new(1.3, theme::STRUCK),
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
                    painter.rect_filled(bar, 1.0, QUOTE_BAR);
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
                                painter.rect_filled(b, 3.0, theme::CHECKBOX_DONE);
                                let tick = [
                                    pos2(b.left() + size * 0.22, b.top() + size * 0.52),
                                    pos2(b.left() + size * 0.42, b.top() + size * 0.72),
                                    pos2(b.left() + size * 0.78, b.top() + size * 0.3),
                                ];
                                painter.line(tick.to_vec(), Stroke::new(2.0, BACKGROUND));
                            } else {
                                painter.rect_stroke(
                                    b,
                                    3.0,
                                    Stroke::new(1.5, LIST_MARKER),
                                    StrokeKind::Inside,
                                );
                            }
                            self.checkboxes.push((b.expand(3.0), at, checked));
                        }
                        None => {
                            let marker = list_marker(doc, parse, c, &p.leaf.containers);
                            self.text
                                .draw_line(meshes, &marker, pos2(x + 4.0, y), LIST_MARKER);
                        }
                    }
                }
                BlockKind::FootnoteDefinition => {
                    if let Some(label) = footnote_label(doc, parse, c, &p.leaf.block) {
                        let y = top + p.body.seg_tops.first().copied().unwrap_or(0.0);
                        let marker = self.fit_marker(&label, FOOTNOTE_INDENT - 6.0);
                        self.text
                            .draw_line(meshes, &marker, pos2(x + 2.0, y), LIST_MARKER);
                    }
                }
                _ => {}
            }
            x += container_indent(c);
        }
        match p.body.layout.style {
            LeafStyle::Code | LeafStyle::Html => {
                let bg = Rect::from_min_max(pos2(x, top + 2.0), pos2(right, top + p.height - 2.0));
                painter.rect_filled(bg, 4.0, CODE_BACKGROUND);
            }
            LeafStyle::Rule if p.body.layout.segments.iter().all(|s| s.text.is_empty()) => {
                let y = top + p.height / 2.0;
                painter.line_segment([pos2(x, y), pos2(right, y)], Stroke::new(1.5, RULE));
            }
            _ => {}
        }
        let text_left = frame.left + p.indent;
        if let Some(grid) = &p.table {
            let origin = pos2(text_left, top + TABLE_PAD);
            let r = Rect::from_min_size(origin, vec2(grid.width(), grid.height()));
            if grid.has_head && !grid.row_h.is_empty() {
                let head = Rect::from_min_size(origin, vec2(grid.width(), grid.row_h[0]));
                painter.rect_filled(head, 0.0, CODE_BACKGROUND);
            }
            let line = Stroke::new(1.0, QUOTE_BAR);
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
            painter.rect_filled(r, 4.0, CODE_BACKGROUND);
            painter.rect_stroke(r, 4.0, Stroke::new(1.0, QUOTE_BAR), StrokeKind::Inside);
            let label_pos = pos2(r.left() + 10.0, r.center().y - self.text.row_height() / 2.0);
            self.text.draw_line(meshes, &label, label_pos, MARKUP);
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
}
