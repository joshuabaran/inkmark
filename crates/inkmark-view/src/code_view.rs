//! The raw Markdown pane: a virtualized, soft-wrapping editor over a
//! [`Document`], drawn with [`TextRenderer`].

use std::borrow::Cow;
use std::ops::Range;
use std::time::{Duration, Instant};

use egui::output::IMEOutput;
use egui::{
    CursorIcon, Event, EventFilter, IMEPurpose, Id, ImeEvent, Key, Modifiers, Pos2, Rect, Response,
    Sense, Ui, pos2, vec2,
};
use inkmark_buffer::{Bias, Change, Document, Edit, EditKind, Selection};
use inkmark_parse::{ParseOutput, ParseState};
use inkmark_text::{GlyphMeshes, ScrollAnchor, SharedFonts, TextConfig, TextRenderer};

use crate::commands::{self, EditPlan};
use crate::lines::SCROLLBAR_WIDTH;
use crate::lines::{LineIndex, ScrollPos, Synced};
use crate::motion;
use crate::theme::{self, BACKGROUND, CARET, SELECTION, TEXT};

const PADDING: f32 = 12.0;
const CARET_WIDTH: f32 = 2.0;
/// Width of the highlight drawn for a selected newline.
const NEWLINE_WIDTH: f32 = 6.0;
const MULTI_CLICK: Duration = Duration::from_millis(400);
const INDENT: &str = "    ";
/// Frames we keep nudging the scroll after a caret move, while estimated
/// heights between here and the caret get measured.
const REVEAL_FRAMES: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Granularity {
    Char,
    Word,
    Line,
}

struct Drag {
    granularity: Granularity,
    /// The word or line first clicked, kept selected while dragging.
    origin: Range<usize>,
}

pub struct CodeView {
    id: Id,
    text: TextRenderer,
    lines: LineIndex,
    selection: Selection,
    /// Remembered x for repeated up/down so the caret keeps its column.
    preferred_x: Option<f32>,
    preedit: String,
    drag: Option<Drag>,
    last_press: Option<(Instant, Pos2, u8)>,
    reveal_caret: u8,
    /// Set when the user scrolls (wheel, scrollbar, caret moves), for sync.
    scrolled: bool,
    /// Height of the visible area last frame, for the minimap.
    viewport: f32,
    /// Per-pane minimap toggle; survives mode switches with the view.
    pub show_minimap: bool,
    /// The selection was set from outside in current offsets: don't map it
    /// through edits on the next sync.
    selection_current: bool,
    /// A scroll position set from outside, applied after the next sync.
    pending_scroll: Option<ScrollPos>,
    /// A link was Ctrl+clicked at this offset; the app follows it.
    follow: Option<usize>,
    /// The caret at this offset is at the end of a row that wrapped
    /// mid-word (End, or a click past the row), not the start of the next
    /// row. Stale as soon as the caret is anywhere else.
    upstream_at: Option<usize>,
    /// Whether the last `offset_at` hit was such a row end.
    hit_upstream: bool,
    pub font_size: f32,
    pub line_height: f32,
}

/// Where the text area sits this frame.
#[derive(Clone, Copy)]
struct Frame {
    rect: Rect,
    text_left: f32,
    /// Where text stops: the minimap's left edge, or the scrollbar's.
    text_right: f32,
    bar_left: f32,
    minimap: Option<Rect>,
}

impl CodeView {
    pub fn new(ctx: &egui::Context, id: Id) -> Self {
        Self::with_fonts(inkmark_text::Fonts::shared(ctx), id)
    }

    /// A view drawing with fonts shared with other panes.
    pub fn with_fonts(fonts: SharedFonts, id: Id) -> Self {
        Self {
            id,
            text: TextRenderer::with_fonts(fonts),
            lines: LineIndex::new(),
            selection: Selection::default(),
            preferred_x: None,
            preedit: String::new(),
            drag: None,
            last_press: None,
            reveal_caret: 0,
            scrolled: false,
            viewport: 0.0,
            show_minimap: true,
            selection_current: false,
            pending_scroll: None,
            follow: None,
            upstream_at: None,
            hit_upstream: false,
            font_size: 14.0,
            line_height: 21.0,
        }
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// Moves the selection (e.g. from search or the other pane) and scrolls to it.
    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
        self.selection_current = true;
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
    }

    /// The offset of a link Ctrl+clicked since the last call.
    pub fn take_follow(&mut self) -> Option<usize> {
        self.follow.take()
    }

    /// Shows `selection` without scrolling to it (mirroring the other pane).
    pub fn mirror_selection(&mut self, selection: Selection) {
        self.selection = selection;
        self.selection_current = true;
    }

    pub fn has_focus(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| m.has_focus(self.id))
    }

    /// Where the view is scrolled to, as a source line and fraction.
    pub fn scroll_pos(&self) -> ScrollPos {
        if let Some(pos) = self.pending_scroll {
            return pos;
        }
        let anchor = self.lines.anchor;
        let height = if anchor.line < self.lines.heights.len() {
            self.lines.heights.height(anchor.line)
        } else {
            0.0
        };
        ScrollPos {
            line: anchor.line,
            frac: if height > 0.0 {
                anchor.offset / height
            } else {
                0.0
            },
        }
    }

    /// Scrolls to `pos` (e.g. to follow the other pane).
    /// The position is current for the document as it is now; it's applied
    /// once the view has caught up with the document (so a pane that was
    /// hidden doesn't shift it through the same edits again).
    pub fn set_scroll_pos(&mut self, pos: ScrollPos) {
        self.pending_scroll = Some(pos);
    }

    fn apply_scroll_pos(&mut self, pos: ScrollPos) {
        let heights = &self.lines.heights;
        if pos.line < heights.len() {
            self.lines.anchor = ScrollAnchor {
                line: pos.line,
                offset: pos.frac * heights.height(pos.line),
            };
        }
    }

    /// Whether the user scrolled this view since the last call.
    pub fn take_scrolled(&mut self) -> bool {
        std::mem::take(&mut self.scrolled)
    }

    /// Forgets all per-document state, for after a new document is loaded.
    pub fn reset(&mut self) {
        self.lines.reset();
        self.selection = Selection::default();
        self.preferred_x = None;
        self.preedit.clear();
        self.drag = None;
    }

    /// Gives up keyboard focus (e.g. while a dialog is open).
    pub fn release_focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.surrender_focus(self.id));
    }

    pub fn request_focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.request_focus(self.id));
    }

    /// Draws and edits `doc`. `parse`, when given, is brought up to date
    /// after this frame's edits and colors the Markdown syntax.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        doc: &mut Document,
        parse: Option<&mut ParseState>,
    ) -> Response {
        let rect = ui.available_rect_before_wrap();
        ui.advance_cursor_after_rect(rect);
        let bar_left = rect.right() - SCROLLBAR_WIDTH;
        let text_right = if self.show_minimap {
            bar_left - inkmark_minimap::WIDTH
        } else {
            bar_left
        };
        let frame = Frame {
            rect,
            text_left: rect.left() + PADDING,
            text_right,
            bar_left,
            minimap: self.show_minimap.then(|| {
                Rect::from_min_max(pos2(text_right, rect.top()), pos2(bar_left, rect.bottom()))
            }),
        };
        let text_rect = Rect::from_min_max(rect.min, pos2(frame.text_right, rect.bottom()));
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
        let focused = response.has_focus();
        let viewport = rect.height();
        self.viewport = viewport;

        let config = TextConfig {
            monospace: true,
            font_size: self.font_size,
            line_height: self.line_height,
            wrap_width: Some((frame.text_right - frame.text_left - PADDING).max(40.0)),
        };
        if self.text.begin_frame(config, ui.ctx().pixels_per_point()) {
            self.lines.invalidate();
        }
        self.sync(doc, true);
        if let Some(pos) = self.pending_scroll.take() {
            self.apply_scroll_pos(pos);
        }

        if focused {
            self.handle_events(ui, doc, viewport);
        } else {
            self.preedit.clear();
        }
        let current = parse
            .as_deref()
            .map(ParseState::output)
            .filter(|p| p.map.len() == doc.len());
        let link = crate::link_under_pointer(ui, &response, current, |pos| {
            self.offset_at(doc, frame, pos)
        });
        // Over a link with Ctrl held, clicks follow it and nothing else.
        match link {
            Some((at, pressed)) => {
                if pressed {
                    self.follow = Some(at);
                }
            }
            None => self.handle_pointer(ui, &response, doc, frame),
        }
        let bar = Rect::from_min_max(pos2(frame.bar_left, rect.top()), rect.max);
        let mut minimap_hovered = false;
        if let Some(r) = frame.minimap {
            let (scrolled, hovered) = self.lines.minimap_input(ui, self.id, r, viewport);
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
            self.scroll_caret_into_view(ui, doc, viewport);
        }
        let parse = parse.map(|p| {
            if let Some(wait) = p.update(doc) {
                ui.ctx().request_repaint_after(wait);
            }
            &*p
        });
        let caret = self.paint(ui, doc, parse.map(ParseState::output), frame, focused);

        if focused {
            let to_global = ui
                .ctx()
                .layer_transform_to_global(ui.layer_id())
                .unwrap_or_default();
            let cursor_rect = caret.unwrap_or(Rect::from_min_size(rect.min, vec2(1.0, 1.0)));
            ui.output_mut(|o| {
                o.ime = Some(IMEOutput {
                    purpose: IMEPurpose::Normal,
                    rect: to_global * rect,
                    cursor_rect: to_global * cursor_rect,
                    should_interrupt_composition: false,
                })
            });
        }
        response
    }

    // ---- document sync ----------------------------------------------------

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
        self.clamp_selection(doc);
    }

    fn clamp_selection(&mut self, doc: &Document) {
        self.selection.anchor = self.selection.anchor.min(doc.len());
        self.selection.head = self.selection.head.min(doc.len());
    }

    // ---- editing ------------------------------------------------------------

    fn after_edit(&mut self, doc: &Document) {
        self.sync(doc, false);
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
    }

    /// Replaces the selection with `text` and puts the caret after it.
    fn insert(&mut self, doc: &mut Document, text: &str, kind: EditKind) {
        let range = self.selection.range();
        if range.is_empty() && text.is_empty() {
            return;
        }
        let after = Selection::caret(range.start + text.len());
        if doc
            .apply(
                vec![Edit::replace(range, text)],
                self.selection,
                after,
                kind,
            )
            .is_ok()
        {
            self.selection = after;
            self.after_edit(doc);
        }
    }

    fn delete(&mut self, doc: &mut Document, range: Range<usize>, kind: EditKind) {
        if range.is_empty() {
            return;
        }
        let after = Selection::caret(range.start);
        if doc
            .apply(vec![Edit::delete(range)], self.selection, after, kind)
            .is_ok()
        {
            self.selection = after;
            self.after_edit(doc);
        }
    }

    /// Applies line-wise edits (indent/outdent) and maps the selection through them.
    fn apply_line_edits(&mut self, doc: &mut Document, edits: Vec<Edit>) {
        if edits.is_empty() {
            return;
        }
        let map = |offset: usize| {
            edits.iter().fold(offset, |o, e| {
                Change {
                    start: e.range.start,
                    old_end: e.range.end,
                    new_end: e.range.start + e.insert.len(),
                    ..Change::default()
                }
                .map(o, Bias::Right)
            })
        };
        let after = Selection {
            anchor: map(self.selection.anchor),
            head: map(self.selection.head),
        };
        if doc
            .apply(edits, self.selection, after, EditKind::Other)
            .is_ok()
        {
            self.selection = after;
            self.after_edit(doc);
        }
    }

    /// Lines touched by the selection; a selection ending at a line start
    /// doesn't include that line.
    fn selected_lines(&self, doc: &Document) -> Range<usize> {
        let range = self.selection.range();
        let first = doc.byte_to_line(range.start);
        let mut last = doc.byte_to_line(range.end);
        if last > first && doc.line_to_byte(last) == range.end {
            last -= 1;
        }
        first..last + 1
    }

    fn indent(&mut self, doc: &mut Document) {
        // Later lines first, so earlier offsets stay valid.
        let edits = self
            .selected_lines(doc)
            .rev()
            .map(|l| Edit::insert(doc.line_to_byte(l), INDENT))
            .collect();
        self.apply_line_edits(doc, edits);
    }

    fn outdent(&mut self, doc: &mut Document) {
        let edits = self
            .selected_lines(doc)
            .rev()
            .filter_map(|l| {
                let start = doc.line_to_byte(l);
                let text = doc.slice(doc.line_range(l));
                let width = if text.starts_with('\t') {
                    1
                } else {
                    text.bytes()
                        .take(INDENT.len())
                        .take_while(|&b| b == b' ')
                        .count()
                };
                (width > 0).then(|| Edit::delete(start..start + width))
            })
            .collect();
        self.apply_line_edits(doc, edits);
    }

    fn apply_plan(&mut self, doc: &mut Document, plan: EditPlan) {
        if !plan.edits.is_empty()
            && doc
                .apply(plan.edits, self.selection, plan.selection, plan.kind)
                .is_ok()
        {
            self.selection = plan.selection;
            self.after_edit(doc);
        }
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(selection) = doc.undo() {
            self.selection = selection;
            self.after_edit(doc);
        }
    }

    fn redo(&mut self, doc: &mut Document) {
        if let Some(selection) = doc.redo() {
            self.selection = selection;
            self.after_edit(doc);
        }
    }

    // ---- caret movement -----------------------------------------------------

    fn move_to(&mut self, doc: &mut Document, target: usize, extend: bool) {
        if extend {
            self.selection.head = target;
        } else {
            self.selection = Selection::caret(target);
        }
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
        doc.seal_undo_step();
    }

    fn upstream(&self) -> bool {
        self.upstream_at == Some(self.selection.head)
    }

    fn line_text<'d>(doc: &'d Document, line: usize) -> Cow<'d, str> {
        doc.slice(doc.line_range(line))
    }

    /// Moves the caret `rows` visual rows up (negative) or down, keeping its x.
    fn move_vertical(&mut self, doc: &mut Document, rows: i32, extend: bool) {
        let mut head = self.selection.head;
        let mut line = doc.byte_to_line(head);
        let mut geometry = self.text.geometry(&Self::line_text(doc, line));
        let mut row = geometry.row_of_affine(head - doc.line_to_byte(line), self.upstream());
        let mut upstream = false;
        let x = self
            .preferred_x
            .unwrap_or_else(|| geometry.caret_x(row, head - doc.line_to_byte(line)));
        for _ in 0..rows.unsigned_abs() {
            if rows < 0 {
                if row > 0 {
                    row -= 1;
                } else if line > 0 {
                    line -= 1;
                    geometry = self.text.geometry(&Self::line_text(doc, line));
                    row = geometry.rows.len() - 1;
                } else {
                    head = 0;
                    break;
                }
            } else if row + 1 < geometry.rows.len() {
                row += 1;
            } else if line + 1 < doc.line_count() {
                line += 1;
                geometry = self.text.geometry(&Self::line_text(doc, line));
                row = 0;
            } else {
                head = doc.len();
                break;
            }
            let (d, up) = geometry.hit_row_affine(row, x);
            head = doc.line_to_byte(line) + d;
            upstream = up;
        }
        self.move_to(doc, head, extend);
        self.upstream_at = upstream.then_some(head);
        self.preferred_x = Some(x);
    }

    /// Start (`end == false`) or end of the caret's visual row, and
    /// whether that end is upstream (see `upstream_at`).
    fn row_edge(&mut self, doc: &Document, end: bool) -> (usize, bool) {
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        let start = doc.line_to_byte(line);
        let geometry = self.text.geometry(&Self::line_text(doc, line));
        let row = geometry.row_of_affine(head - start, self.upstream());
        let (d, upstream) = if end {
            geometry.hit_row_affine(row, f32::INFINITY)
        } else {
            (geometry.rows[row].start, false)
        };
        (start + d, upstream)
    }

    // ---- input ----------------------------------------------------------------

    fn handle_events(&mut self, ui: &Ui, doc: &mut Document, viewport: f32) {
        let events = ui.input(|i| i.events.clone());
        for event in events {
            match event {
                Event::Text(text) if self.preedit.is_empty() => {
                    self.insert(doc, &text, EditKind::Typing);
                }
                Event::Paste(text) => {
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    self.insert(doc, &text, EditKind::Other);
                }
                Event::Copy | Event::Cut => {
                    let range = self.selection.range();
                    if !range.is_empty() {
                        ui.ctx().copy_text(doc.slice(range.clone()).into_owned());
                        if matches!(event, Event::Cut) {
                            self.delete(doc, range, EditKind::Other);
                        }
                    }
                }
                Event::Ime(ImeEvent::Preedit { text, .. }) => {
                    if self.preedit.is_empty() && !text.is_empty() {
                        let range = self.selection.range();
                        self.delete(doc, range, EditKind::Other);
                    }
                    self.preedit = text;
                    self.reveal_caret = REVEAL_FRAMES;
                }
                Event::Ime(ImeEvent::Commit(text)) => {
                    self.preedit.clear();
                    self.insert(doc, &text, EditKind::Typing);
                }
                Event::Ime(ImeEvent::DeleteSurrounding {
                    before_chars,
                    after_chars,
                }) => {
                    let mut range = self.selection.range();
                    for _ in 0..before_chars {
                        range.start = doc.prev_char_boundary(range.start);
                    }
                    for _ in 0..after_chars {
                        range.end = doc.next_char_boundary(range.end);
                    }
                    self.selection = Selection::caret(self.selection.range().start);
                    self.delete(doc, range, EditKind::Other);
                }
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if self.preedit.is_empty() => self.handle_key(doc, key, modifiers, viewport),
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, doc: &mut Document, key: Key, modifiers: Modifiers, viewport: f32) {
        let (cmd, shift) = (modifiers.command, modifiers.shift);
        let range = self.selection.range();
        let head = self.selection.head;
        match key {
            Key::ArrowLeft if !shift && !range.is_empty() => self.move_to(doc, range.start, false),
            Key::ArrowRight if !shift && !range.is_empty() => self.move_to(doc, range.end, false),
            Key::ArrowLeft => {
                let target = if cmd {
                    motion::prev_word(doc, head)
                } else {
                    motion::prev_grapheme(doc, head)
                };
                self.move_to(doc, target, shift);
            }
            Key::ArrowRight => {
                let target = if cmd {
                    motion::next_word(doc, head)
                } else {
                    motion::next_grapheme(doc, head)
                };
                self.move_to(doc, target, shift);
            }
            Key::ArrowUp => self.move_vertical(doc, -1, shift),
            Key::ArrowDown => self.move_vertical(doc, 1, shift),
            Key::PageUp | Key::PageDown => {
                let rows = (viewport / self.text.row_height()).floor().max(1.0) as i32 - 1;
                let rows = if key == Key::PageUp { -rows } else { rows };
                self.move_vertical(doc, rows, shift);
            }
            Key::Home if cmd => self.move_to(doc, 0, shift),
            Key::End if cmd => self.move_to(doc, doc.len(), shift),
            Key::Home => {
                let (target, _) = self.row_edge(doc, false);
                self.move_to(doc, target, shift);
            }
            Key::End => {
                let (target, upstream) = self.row_edge(doc, true);
                self.move_to(doc, target, shift);
                self.upstream_at = upstream.then_some(target);
            }
            Key::Backspace if !range.is_empty() => self.delete(doc, range, EditKind::Deleting),
            Key::Delete if !range.is_empty() => self.delete(doc, range, EditKind::Deleting),
            Key::Backspace => {
                let start = if cmd {
                    motion::prev_word(doc, head)
                } else {
                    motion::prev_grapheme(doc, head)
                };
                self.delete(doc, start..head, EditKind::Deleting);
            }
            Key::Delete => {
                let end = if cmd {
                    motion::next_word(doc, head)
                } else {
                    motion::next_grapheme(doc, head)
                };
                self.delete(doc, head..end, EditKind::Deleting);
            }
            Key::Enter if cmd => {
                self.apply_plan(doc, commands::toggle_task(doc, self.selection));
            }
            Key::X if cmd && shift => {
                self.apply_plan(
                    doc,
                    commands::toggle_wrap(doc, self.selection, "~~", &["~"]),
                );
            }
            Key::Enter => self.insert(doc, "\n", EditKind::Typing),
            Key::Tab if shift => self.outdent(doc),
            Key::Tab if self.selected_lines(doc).len() > 1 => self.indent(doc),
            Key::Tab => self.insert(doc, INDENT, EditKind::Typing),
            Key::A if cmd => {
                self.selection = Selection {
                    anchor: 0,
                    head: doc.len(),
                };
                doc.seal_undo_step();
            }
            Key::B if cmd => self.apply_plan(
                doc,
                commands::toggle_wrap(doc, self.selection, "**", &["__"]),
            ),
            Key::I if cmd => {
                self.apply_plan(doc, commands::toggle_wrap(doc, self.selection, "*", &["_"]))
            }
            Key::Backtick if cmd => {
                self.apply_plan(doc, commands::toggle_wrap(doc, self.selection, "`", &[]))
            }
            Key::K if cmd => self.apply_plan(doc, commands::insert_link(doc, self.selection)),
            _ if cmd && modifiers.alt && commands::heading_level(key).is_some() => {
                let level = commands::heading_level(key).expect("checked");
                self.apply_plan(doc, commands::set_heading(doc, self.selection, level));
            }
            Key::Z if cmd && shift => self.redo(doc),
            Key::Z if cmd => self.undo(doc),
            Key::Y if cmd => self.redo(doc),
            Key::Escape if !range.is_empty() => self.move_to(doc, head, false),
            _ => {}
        }
    }

    /// Document offset under a screen position.
    fn offset_at(&mut self, doc: &Document, frame: Frame, pos: Pos2) -> usize {
        let y =
            self.lines.heights.anchor_y(self.lines.anchor) + f64::from(pos.y - frame.rect.top());
        let at = self.lines.heights.line_at(y);
        let text = Self::line_text(doc, at.line);
        let height = self.text.line_height(&text);
        self.lines.heights.set_measured(at.line, height);
        let geometry = self.text.geometry(&text);
        let (d, upstream) = geometry.hit_affine(vec2(pos.x - frame.text_left, at.offset));
        self.hit_upstream = upstream;
        doc.line_to_byte(at.line) + d
    }

    fn handle_pointer(&mut self, ui: &Ui, response: &Response, doc: &mut Document, frame: Frame) {
        let (pressed, down, pos, modifiers) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_down(),
                i.pointer.interact_pos(),
                i.modifiers,
            )
        });
        let Some(pos) = pos else {
            return self.end_drag_if_released(down);
        };
        if pressed && response.hovered() {
            let now = Instant::now();
            let count = match self.last_press {
                Some((t, p, n)) if now - t < MULTI_CLICK && p.distance(pos) < 4.0 => n % 3 + 1,
                _ => 1,
            };
            self.last_press = Some((now, pos, count));
            let at = self.offset_at(doc, frame, pos);
            let granularity = match count {
                1 => Granularity::Char,
                2 => Granularity::Word,
                _ => Granularity::Line,
            };
            let origin = match granularity {
                Granularity::Char => at..at,
                Granularity::Word => motion::word_at(doc, at),
                Granularity::Line => motion::line_at(doc, at),
            };
            self.upstream_at =
                (granularity == Granularity::Char && self.hit_upstream).then_some(at);
            if modifiers.shift && granularity == Granularity::Char {
                self.selection.head = at;
            } else {
                self.selection = Selection {
                    anchor: origin.start,
                    head: origin.end,
                };
            }
            self.drag = Some(Drag {
                granularity,
                origin,
            });
            self.preferred_x = None;
            self.preedit.clear();
            doc.seal_undo_step();
        } else if down && let Some(drag) = &self.drag {
            let (granularity, origin) = (drag.granularity, drag.origin.clone());
            // Drag past the top or bottom edge scrolls.
            let overshoot = if pos.y < frame.rect.top() {
                pos.y - frame.rect.top()
            } else if pos.y > frame.rect.bottom() {
                pos.y - frame.rect.bottom()
            } else {
                0.0
            };
            if overshoot != 0.0 {
                self.lines.anchor = self.lines.heights.scroll_by(
                    self.lines.anchor,
                    overshoot * 0.5,
                    frame.rect.height(),
                );
                ui.ctx().request_repaint();
                self.scrolled = true;
            }
            let at = self.offset_at(doc, frame, pos);
            // Word and line drags grow by whole units and keep the first one selected.
            let unit = match granularity {
                Granularity::Char => {
                    self.selection.head = at;
                    self.upstream_at = self.hit_upstream.then_some(at);
                    return self.end_drag_if_released(down);
                }
                Granularity::Word => motion::word_at(doc, at),
                Granularity::Line => motion::line_at(doc, at),
            };
            self.selection = if unit.start < origin.start {
                Selection {
                    anchor: origin.end,
                    head: unit.start,
                }
            } else {
                Selection {
                    anchor: origin.start,
                    head: unit.end.max(origin.end),
                }
            };
        }
        self.end_drag_if_released(down);
    }

    fn end_drag_if_released(&mut self, down: bool) {
        if !down {
            self.drag = None;
        }
    }

    fn scroll_caret_into_view(&mut self, ui: &Ui, doc: &Document, viewport: f32) {
        self.reveal_caret -= 1;
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        let text = Self::line_text(doc, line);
        let height = self.text.line_height(&text);
        self.lines.heights.set_measured(line, height);
        let caret = self.text.geometry(&text).caret_rect_affine(
            head - doc.line_to_byte(line),
            CARET_WIDTH,
            self.upstream(),
        );
        let top = self.lines.heights.offset_of(line) + f64::from(caret.top());
        let bottom = top + f64::from(caret.height());
        let view_top = self.lines.heights.anchor_y(self.lines.anchor);
        if top < view_top {
            self.lines.anchor = self.lines.heights.line_at(top);
        } else if bottom > view_top + f64::from(viewport) {
            self.lines.anchor = self.lines.heights.line_at(bottom - f64::from(viewport));
        } else {
            self.reveal_caret = 0;
            return;
        }
        self.scrolled = true;
        if self.reveal_caret > 0 {
            ui.ctx().request_repaint();
        }
    }

    // ---- painting -------------------------------------------------------------

    /// Paints the visible lines; returns the caret rect if it is on screen.
    fn paint(
        &mut self,
        ui: &Ui,
        doc: &Document,
        parse: Option<&ParseOutput>,
        frame: Frame,
        focused: bool,
    ) -> Option<Rect> {
        let rect = frame.rect;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BACKGROUND);

        let selection = self.selection.range();
        let caret_line = doc.byte_to_line(self.selection.head);
        let mut meshes = GlyphMeshes::default();
        let mut highlights = Vec::new();
        let mut colors = Vec::new();
        let mut caret = None;
        let mut y = rect.top() - self.lines.anchor.offset;
        let mut line = self.lines.anchor.line;
        while y < rect.bottom() && line < doc.line_count() {
            let range = doc.line_range(line);
            let text = doc.slice(range.clone());
            let height = self.text.line_height(&text);
            self.lines.heights.set_measured(line, height);
            if line == self.lines.anchor.line && self.lines.anchor.offset > height {
                self.lines.anchor.offset = height;
            }
            let origin = pos2(frame.text_left, y);

            let selected = selection.start <= range.end && selection.end > range.start;
            if (selected && !selection.is_empty()) || line == caret_line {
                let geometry = self.text.geometry(&text);
                if selected && !selection.is_empty() {
                    let local = selection.start.saturating_sub(range.start)
                        ..selection.end.min(range.end) - range.start;
                    let mut rects = Vec::new();
                    geometry.selection_rects(
                        local,
                        selection.end > range.end,
                        NEWLINE_WIDTH,
                        &mut rects,
                    );
                    highlights.extend(rects.into_iter().map(|r| r.translate(origin.to_vec2())));
                }
                if line == caret_line {
                    let r = geometry.caret_rect_affine(
                        self.selection.head - range.start,
                        CARET_WIDTH,
                        self.upstream(),
                    );
                    caret = Some(r.translate(origin.to_vec2()));
                }
            }
            colors.clear();
            if let Some(parse) = parse
                && parse.map.len() == doc.len()
            {
                for span in parse.map.spans_in(range.clone()) {
                    let local = span.range.start.max(range.start) - range.start
                        ..span.range.end.min(range.end) - range.start;
                    if let Some(color) = theme::code_color(&span)
                        && !local.is_empty()
                    {
                        colors.push((local, color));
                    }
                }
            }
            self.text
                .draw_line_colored(&mut meshes, &text, origin, TEXT, &colors);
            y += height;
            line += 1;
        }
        for r in highlights {
            painter.rect_filled(r, 0.0, SELECTION);
        }
        if let Some(c) = caret
            && !self.preedit.is_empty()
        {
            // Composition text is drawn over the line, underlined, until committed.
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
        self.text.end_frame(meshes, &painter);
        if focused && let Some(c) = caret {
            painter.rect_filled(c, 0.0, CARET);
        }
        self.lines.paint_scrollbar(
            &painter,
            Rect::from_min_max(pos2(frame.bar_left, rect.top()), rect.max),
        );
        if let Some(r) = frame.minimap {
            self.paint_minimap(&painter, ui, doc, parse, r);
        }
        caret.filter(|c| rect.intersects(*c))
    }

    /// One bar per source line in the minimap's window: indent to length,
    /// colored by the syntax the line starts with.
    fn paint_minimap(
        &self,
        painter: &egui::Painter,
        ui: &Ui,
        doc: &Document,
        parse: Option<&ParseOutput>,
        rect: Rect,
    ) {
        /// Characters across the minimap's full width.
        const COLUMNS: f32 = 100.0;
        let heights = &self.lines.heights;
        if heights.is_empty() {
            return;
        }
        let m = self.lines.minimap(rect, self.viewport);
        m.paint_background(painter);
        let parse = parse.filter(|p| p.map.len() == doc.len());
        let (top, bottom) = m.window();
        let mut line = heights.line_at(top).line;
        let mut y = heights.offset_of(line);
        while y < bottom && line < doc.line_count().min(heights.len()) {
            let h = f64::from(heights.height(line));
            let (mut indent, mut indent_bytes, mut len) = (0.0f32, 0, 0.0f32);
            let mut in_indent = true;
            for c in doc.rope().line(line).chars().take(COLUMNS as usize) {
                if c == '\n' {
                    break;
                }
                let w = if c == '\t' { 4.0 } else { 1.0 };
                if in_indent && c.is_whitespace() {
                    indent += w;
                    indent_bytes += c.len_utf8();
                } else {
                    in_indent = false;
                }
                len += w;
            }
            if len > indent {
                let start = doc.line_to_byte(line) + indent_bytes;
                let color = parse
                    .and_then(|p| p.map.spans_in(start..start + 1).into_iter().next())
                    .and_then(|s| theme::code_color(&s))
                    .map_or(theme::MINI_TEXT, |c| c.gamma_multiply(0.55));
                m.bar(
                    painter,
                    y + h * 0.2,
                    h * 0.6,
                    (indent / COLUMNS, len.min(COLUMNS) / COLUMNS),
                    color,
                );
            }
            y += h;
            line += 1;
        }
        m.paint_viewport(painter, ui.rect_contains_pointer(rect));
    }
}
