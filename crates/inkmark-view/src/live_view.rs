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
use inkmark_text::{GlyphMeshes, LineGeometry, RichLine, SharedFonts, TextConfig, TextRenderer};

use crate::commands::{self, EditPlan, EnterContext};
use crate::images::{ImageCache, ImageSlot};
use crate::lines::{LineIndex, SCROLLBAR_WIDTH, ScrollPos, Synced};
use crate::live_layout::{self, LeafLayout, LeafStyle};
use crate::motion;
use crate::theme::{
    BACKGROUND, CARET, CODE_BACKGROUND, LIST_MARKER, MARKUP, QUOTE_BAR, RULE, SELECTION, TEXT,
};

const PADDING: f32 = 28.0;
const QUOTE_INDENT: f32 = 22.0;
const ITEM_INDENT: f32 = 28.0;
const CODE_PAD: f32 = 10.0;
const CARET_WIDTH: f32 = 2.0;
const NEWLINE_WIDTH: f32 = 6.0;
/// Space between a paragraph's text and an image below it.
const IMAGE_GAP: f32 = 6.0;
const REVEAL_FRAMES: u8 = 3;
/// Height of a blank source line, in rows.
const BLANK_LINE: f32 = 0.6;

/// A leaf block laid out for this frame.
struct Placed {
    leaf: Leaf,
    layout: LeafLayout,
    first_line: usize,
    last_line: usize,
    /// Text left edge, from the pane's content left.
    indent: f32,
    seg_tops: Vec<f32>,
    geometry: Vec<LineGeometry>,
    /// An image-only paragraph away from the caret shows just its images.
    text_hidden: bool,
    images: Vec<PlacedImage>,
    height: f32,
}

/// An image below a leaf's text, in leaf coordinates.
struct PlacedImage {
    top: f32,
    size: Vec2,
    slot: ImageSlot,
    dest: String,
}

impl Placed {
    /// The segment at height `y` (from the leaf top).
    fn segment_at(&self, y: f32) -> usize {
        self.seg_tops.iter().rposition(|&t| t <= y).unwrap_or(0)
    }
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
            images: None,
            font_size: 16.0,
            line_height: 26.0,
        }
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
    }

    /// Shows `selection` without scrolling to it (mirroring the other pane).
    pub fn mirror_selection(&mut self, selection: Selection) {
        self.selection = selection;
    }

    pub fn reset(&mut self) {
        self.lines.reset();
        self.selection = Selection::default();
        self.preferred_x = None;
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
        let heights = &self.lines.heights;
        let anchor = self.lines.anchor;
        if anchor.line >= heights.len() {
            return ScrollPos::default();
        }
        match leaf_lines(doc, parse, anchor.line) {
            Some((first, last)) if first == anchor.line => {
                let height = heights.height(first).max(1.0);
                let progress = (anchor.offset / height).clamp(0.0, 1.0);
                let lines = progress * (last - first + 1) as f32;
                ScrollPos {
                    line: first + lines as usize,
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
    pub fn set_scroll_pos(&mut self, doc: &Document, parse: &ParseOutput, pos: ScrollPos) {
        let heights = &self.lines.heights;
        if pos.line >= heights.len() {
            return;
        }
        let y = match leaf_lines(doc, parse, pos.line) {
            Some((first, last)) => {
                let progress = ((pos.line - first) as f32 + pos.frac) / (last - first + 1) as f32;
                heights.offset_of(first) + f64::from(progress * heights.height(first))
            }
            None => heights.offset_of(pos.line) + f64::from(pos.frac * heights.height(pos.line)),
        };
        self.lines.anchor = heights.line_at(y);
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
        let frame = Frame {
            rect,
            left: rect.left() + PADDING,
            width: (bar.left() - rect.left() - 2.0 * PADDING).max(80.0),
        };
        let text_rect = Rect::from_min_max(rect.min, pos2(bar.left(), rect.bottom()));
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

        if self.focused {
            self.handle_events(ui, doc, state, frame);
        } else {
            self.preedit.clear();
        }
        let parse = state.output();
        self.handle_pointer(ui, &response, doc, parse, frame);
        if self
            .lines
            .scroll_input(ui, self.id, response.hovered(), bar)
        {
            self.scrolled = true;
        }
        if self.reveal_caret > 0 {
            self.scroll_caret_into_view(ui, doc, parse, frame);
        }
        let caret = self.paint(&painter, doc, parse, frame);
        self.lines.paint_scrollbar(&painter, bar);
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

    /// Source line whose raw syntax is shown: the caret's, while focused.
    fn reveal_range(&self, doc: &Document) -> Option<Range<usize>> {
        self.focused
            .then(|| doc.line_range(doc.byte_to_line(self.selection.head)))
    }

    fn place(
        &mut self,
        doc: &Document,
        map: &inkmark_parse::SourceMap,
        leaf: Leaf,
        width: f32,
    ) -> Placed {
        let layout = live_layout::build(doc, map, &leaf, self.reveal_range(doc));
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
            for image in inline_images(&doc.slice(range)) {
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
        Placed {
            first_line,
            last_line,
            indent: containers + inner,
            height: y + pad_bottom,
            leaf,
            layout,
            seg_tops,
            geometry,
            text_hidden,
            images,
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
        Some(self.place(doc, &parse.map, leaf, width))
    }

    // ---- caret -----------------------------------------------------------------

    /// The caret in document coordinates: (top y, left x from content left, height).
    fn caret_doc(&mut self, doc: &Document, parse: &ParseOutput, frame: Frame) -> (f64, f32, f32) {
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        match self.place_line(doc, parse, line, frame.width) {
            Some(p) => {
                let (seg, d) = p.layout.display_pos(head);
                let r = p.geometry[seg].caret_rect(d, CARET_WIDTH);
                let top = self.lines.heights.offset_of(p.first_line)
                    + f64::from(p.seg_tops[seg] + r.top());
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
                let seg = p.segment_at(local);
                let d = p.geometry[seg].hit(vec2(x - p.indent, local - p.seg_tops[seg]));
                p.layout.source_pos(seg, d)
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
        let Some(leaf) = leaf_at_line(doc, parse, line) else {
            return true;
        };
        let layout = live_layout::build(doc, &parse.map, &leaf, Some(doc.line_range(line)));
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
        let (seg, d) = p.layout.display_pos(head);
        let g = &p.geometry[seg];
        let row = g.row_of(d);
        let d = if end {
            g.hit_row(row, f32::INFINITY)
        } else {
            g.rows[row].start
        };
        p.layout.source_pos(seg, d)
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
                    self.replace_selection(doc, &text, EditKind::Other)
                }
                Event::Text(text) if self.preedit.is_empty() => {
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
                return self.delete(doc, start..sel.head, EditKind::Deleting);
            }
            Key::Delete => {
                let end = self.step(doc, parse, true, cmd);
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
            let p = self.place(doc, &parse.map, leaf, frame.width);
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
            self.draw_leaf(painter, &mut meshes, doc, &p, frame, y, &selection);
            if (p.first_line..=p.last_line).contains(&caret_line) {
                let (seg, d) = p.layout.display_pos(self.selection.head);
                let r = p.geometry[seg].caret_rect(d, CARET_WIDTH);
                caret = Some(r.translate(vec2(frame.left + p.indent, y + p.seg_tops[seg])));
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

    #[allow(clippy::too_many_arguments)]
    fn draw_leaf(
        &mut self,
        painter: &egui::Painter,
        meshes: &mut GlyphMeshes,
        doc: &Document,
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
                    let marker = list_marker(doc, c.range.start, &p.leaf.containers);
                    let y = top + p.seg_tops.first().copied().unwrap_or(0.0);
                    self.text
                        .draw_line(meshes, &marker, pos2(x + 4.0, y), LIST_MARKER);
                }
                _ => {}
            }
            x += container_indent(c);
        }
        match p.layout.style {
            LeafStyle::Code | LeafStyle::Html => {
                let bg = Rect::from_min_max(pos2(x, top + 2.0), pos2(right, top + p.height - 2.0));
                painter.rect_filled(bg, 4.0, CODE_BACKGROUND);
            }
            LeafStyle::Rule if p.layout.segments.iter().all(|s| s.text.is_empty()) => {
                let y = top + p.height / 2.0;
                painter.line_segment([pos2(x, y), pos2(right, y)], Stroke::new(1.5, RULE));
            }
            _ => {}
        }
        let text_left = frame.left + p.indent;
        let leaf_range = p.leaf.block.range.clone();
        let sel_in_leaf = selection.start.max(leaf_range.start)..selection.end.min(leaf_range.end);
        let (s_seg, s_d) = p.layout.display_pos(sel_in_leaf.start);
        let (e_seg, e_d) = p.layout.display_pos(sel_in_leaf.end);
        for (i, seg) in p.layout.segments.iter().enumerate() {
            let origin = pos2(text_left, top + p.seg_tops[i]);
            if !selection.is_empty()
                && sel_in_leaf.start < sel_in_leaf.end
                && (s_seg..=e_seg).contains(&i)
            {
                let start = if i == s_seg { s_d } else { 0 };
                let end = if i == e_seg { e_d } else { seg.text.len() };
                let mut rects = Vec::new();
                p.geometry[i].selection_rects(
                    start..end,
                    i < e_seg || selection.end > leaf_range.end,
                    NEWLINE_WIDTH,
                    &mut rects,
                );
                for r in rects {
                    painter.rect_filled(r.translate(origin.to_vec2()), 0.0, SELECTION);
                }
            }
            if p.text_hidden || (p.layout.style == LeafStyle::Rule && seg.text.is_empty()) {
                continue;
            }
            self.text.draw_rich(
                meshes,
                RichLine {
                    text: &seg.text,
                    runs: &seg.runs,
                    wrap_width: Some(frame.width - p.indent),
                },
                origin,
                TEXT,
                &seg.colors,
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

fn container_indent(c: &inkmark_parse::Block) -> f32 {
    match c.kind {
        BlockKind::BlockQuote => QUOTE_INDENT,
        BlockKind::Item => ITEM_INDENT,
        _ => 0.0,
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

/// "•" (by nesting depth) for bullet items; the source number for ordered ones.
fn list_marker(doc: &Document, item_start: usize, containers: &[inkmark_parse::Block]) -> String {
    let line_end = doc.line_range(doc.byte_to_line(item_start)).end;
    let source = doc.slice(item_start..line_end);
    let number: String = source
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ')')
        .collect();
    if number.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return number;
    }
    let depth = containers
        .iter()
        .filter(|c| matches!(c.kind, BlockKind::List { .. }))
        .count();
    ["•", "◦", "▪"][depth.saturating_sub(1) % 3].to_owned()
}
