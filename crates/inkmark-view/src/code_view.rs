//! The raw Markdown pane: a virtualized, soft-wrapping editor over a
//! [`Document`], drawn with [`TextRenderer`].

use std::borrow::Cow;
use std::ops::Range;
use std::time::{Duration, Instant};

use egui::output::IMEOutput;
use egui::{
    Color32, CursorIcon, Event, EventFilter, IMEPurpose, Id, ImeEvent, Key, Modifiers, Pos2, Rect,
    Response, Sense, Ui, pos2, vec2,
};
use inkmark_buffer::{Bias, Change, Document, Edit, EditKind, Selection};
use inkmark_text::{GlyphMeshes, HeightCache, ScrollAnchor, TextConfig, TextRenderer};

use crate::motion;

const PADDING: f32 = 12.0;
const SCROLLBAR_WIDTH: f32 = 10.0;
const CARET_WIDTH: f32 = 2.0;
/// Width of the highlight drawn for a selected newline.
const NEWLINE_WIDTH: f32 = 6.0;
const MULTI_CLICK: Duration = Duration::from_millis(400);
const INDENT: &str = "    ";
/// Frames we keep nudging the scroll after a caret move, while estimated
/// heights between here and the caret get measured.
const REVEAL_FRAMES: u8 = 3;

const BACKGROUND: Color32 = Color32::from_rgb(22, 22, 26);
const TEXT: Color32 = Color32::from_gray(212);
const SELECTION: Color32 = Color32::from_rgba_premultiplied(38, 60, 98, 120);
const CARET: Color32 = Color32::from_rgb(120, 170, 255);
const SCROLL_TRACK: Color32 = Color32::from_gray(28);
const SCROLL_THUMB: Color32 = Color32::from_gray(80);

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
    heights: HeightCache,
    anchor: ScrollAnchor,
    selection: Selection,
    /// Remembered x for repeated up/down so the caret keeps its column.
    preferred_x: Option<f32>,
    /// Epoch the heights and selection are in sync with; `None` → rebuild.
    synced_epoch: Option<u64>,
    preedit: String,
    drag: Option<Drag>,
    last_press: Option<(Instant, Pos2, u8)>,
    reveal_caret: u8,
    pub font_size: f32,
    pub line_height: f32,
}

/// Where the text area sits this frame.
#[derive(Clone, Copy)]
struct Frame {
    rect: Rect,
    text_left: f32,
    bar_left: f32,
}

impl CodeView {
    pub fn new(ctx: &egui::Context, id: Id) -> Self {
        Self {
            id,
            text: TextRenderer::new(ctx),
            heights: HeightCache::new([]),
            anchor: ScrollAnchor::default(),
            selection: Selection::default(),
            preferred_x: None,
            synced_epoch: None,
            preedit: String::new(),
            drag: None,
            last_press: None,
            reveal_caret: 0,
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
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
    }

    /// Forgets all per-document state, for after a new document is loaded.
    pub fn reset(&mut self) {
        self.synced_epoch = None;
        self.anchor = ScrollAnchor::default();
        self.selection = Selection::default();
        self.preferred_x = None;
        self.preedit.clear();
        self.drag = None;
    }

    pub fn request_focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.request_focus(self.id));
    }

    pub fn show(&mut self, ui: &mut Ui, doc: &mut Document) -> Response {
        let rect = ui.available_rect_before_wrap();
        ui.advance_cursor_after_rect(rect);
        let frame = Frame {
            rect,
            text_left: rect.left() + PADDING,
            bar_left: rect.right() - SCROLLBAR_WIDTH,
        };
        let text_rect = Rect::from_min_max(rect.min, pos2(frame.bar_left, rect.bottom()));
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

        let config = TextConfig {
            monospace: true,
            font_size: self.font_size,
            line_height: self.line_height,
            wrap_width: Some((frame.bar_left - frame.text_left - PADDING).max(40.0)),
        };
        if self.text.begin_frame(config, ui.ctx().pixels_per_point()) {
            self.synced_epoch = None;
        }
        self.sync(doc, true);

        if focused {
            self.handle_events(ui, doc, viewport);
        } else {
            self.preedit.clear();
        }
        self.handle_pointer(ui, &response, doc, frame);
        self.handle_scroll(ui, &response, frame);
        if self.reveal_caret > 0 {
            self.scroll_caret_into_view(ui, doc, viewport);
        }
        let caret = self.paint(ui, doc, frame, focused);

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

    fn rebuild_heights(&mut self, doc: &Document) {
        let text = &self.text;
        self.heights.reset_estimates(
            doc.rope()
                .lines()
                .map(|l| text.estimate_height(l.len_chars())),
        );
        self.anchor.line = self.anchor.line.min(self.heights.len().saturating_sub(1));
        self.clamp_selection(doc);
        self.synced_epoch = Some(doc.epoch());
    }

    /// Brings heights (and, for edits made elsewhere, the selection) up to
    /// date with `doc`.
    fn sync(&mut self, doc: &Document, map_selection: bool) {
        let Some(since) = self.synced_epoch else {
            return self.rebuild_heights(doc);
        };
        if since == doc.epoch() {
            return;
        }
        let Some(changes) = doc.log().changes_since(since) else {
            return self.rebuild_heights(doc);
        };
        let changes: Vec<Change> = changes.copied().collect();
        for c in &changes {
            let lines = c.lines;
            let old = lines.start..lines.start + lines.removed + 1;
            if old.end > self.heights.len() {
                return self.rebuild_heights(doc);
            }
            // Estimates only; real heights arrive when the lines are drawn.
            let estimates: Vec<f32> = (lines.start..lines.start + lines.inserted + 1)
                .map(|l| {
                    if l < doc.line_count() {
                        self.text.estimate_height(doc.rope().line(l).len_chars())
                    } else {
                        self.text.row_height()
                    }
                })
                .collect();
            self.heights.splice(old, estimates.into_iter());
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
            if map_selection {
                self.selection.anchor = c.map(self.selection.anchor, Bias::Left);
                self.selection.head = c.map(self.selection.head, Bias::Left);
            }
        }
        if self.heights.len() != doc.line_count() {
            return self.rebuild_heights(doc);
        }
        self.clamp_selection(doc);
        self.synced_epoch = Some(doc.epoch());
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

    fn line_text<'d>(doc: &'d Document, line: usize) -> Cow<'d, str> {
        doc.slice(doc.line_range(line))
    }

    /// Moves the caret `rows` visual rows up (negative) or down, keeping its x.
    fn move_vertical(&mut self, doc: &mut Document, rows: i32, extend: bool) {
        let mut head = self.selection.head;
        let mut line = doc.byte_to_line(head);
        let mut geometry = self.text.geometry(&Self::line_text(doc, line));
        let mut row = geometry.row_of(head - doc.line_to_byte(line));
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
            head = doc.line_to_byte(line) + geometry.hit_row(row, x);
        }
        self.move_to(doc, head, extend);
        self.preferred_x = Some(x);
    }

    /// Start (`end == false`) or end of the caret's visual row.
    fn row_edge(&mut self, doc: &Document, end: bool) -> usize {
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        let start = doc.line_to_byte(line);
        let geometry = self.text.geometry(&Self::line_text(doc, line));
        let row = geometry.row_of(head - start);
        start
            + if end {
                geometry.hit_row(row, f32::INFINITY)
            } else {
                geometry.rows[row].start
            }
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
                let target = self.row_edge(doc, false);
                self.move_to(doc, target, shift);
            }
            Key::End => {
                let target = self.row_edge(doc, true);
                self.move_to(doc, target, shift);
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
            Key::Z if cmd && shift => self.redo(doc),
            Key::Z if cmd => self.undo(doc),
            Key::Y if cmd => self.redo(doc),
            Key::Escape if !range.is_empty() => self.move_to(doc, head, false),
            _ => {}
        }
    }

    /// Document offset under a screen position.
    fn offset_at(&mut self, doc: &Document, frame: Frame, pos: Pos2) -> usize {
        let y = self.heights.anchor_y(self.anchor) + f64::from(pos.y - frame.rect.top());
        let at = self.heights.line_at(y);
        let text = Self::line_text(doc, at.line);
        let height = self.text.line_height(&text);
        self.heights.set_measured(at.line, height);
        let geometry = self.text.geometry(&text);
        doc.line_to_byte(at.line) + geometry.hit(vec2(pos.x - frame.text_left, at.offset))
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
                self.anchor =
                    self.heights
                        .scroll_by(self.anchor, overshoot * 0.5, frame.rect.height());
                ui.ctx().request_repaint();
            }
            let at = self.offset_at(doc, frame, pos);
            // Word and line drags grow by whole units and keep the first one selected.
            let unit = match granularity {
                Granularity::Char => {
                    self.selection.head = at;
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

    fn handle_scroll(&mut self, ui: &Ui, response: &Response, frame: Frame) {
        let viewport = frame.rect.height();
        let bar = Rect::from_min_max(pos2(frame.bar_left, frame.rect.top()), frame.rect.max);
        let bar_response = ui.interact(bar, self.id.with("scrollbar"), Sense::click_and_drag());
        if (bar_response.dragged() || bar_response.clicked())
            && let Some(pos) = bar_response.interact_pointer_pos()
        {
            let frac = ((pos.y - bar.top()) / bar.height()).clamp(0.0, 1.0);
            let target = f64::from(frac) * self.heights.total() - f64::from(viewport) / 2.0;
            self.anchor = self.heights.line_at(target.max(0.0));
            self.anchor = self.heights.scroll_by(self.anchor, 0.0, viewport);
        }
        let hovered = response.hovered() || bar_response.hovered();
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if hovered && wheel != 0.0 {
            self.anchor = self.heights.scroll_by(self.anchor, -wheel, viewport);
        }
    }

    fn scroll_caret_into_view(&mut self, ui: &Ui, doc: &Document, viewport: f32) {
        self.reveal_caret -= 1;
        let head = self.selection.head;
        let line = doc.byte_to_line(head);
        let text = Self::line_text(doc, line);
        let height = self.text.line_height(&text);
        self.heights.set_measured(line, height);
        let caret = self
            .text
            .geometry(&text)
            .caret_rect(head - doc.line_to_byte(line), CARET_WIDTH);
        let top = self.heights.offset_of(line) + f64::from(caret.top());
        let bottom = top + f64::from(caret.height());
        let view_top = self.heights.anchor_y(self.anchor);
        if top < view_top {
            self.anchor = self.heights.line_at(top);
        } else if bottom > view_top + f64::from(viewport) {
            self.anchor = self.heights.line_at(bottom - f64::from(viewport));
        } else {
            self.reveal_caret = 0;
            return;
        }
        if self.reveal_caret > 0 {
            ui.ctx().request_repaint();
        }
    }

    // ---- painting -------------------------------------------------------------

    /// Paints the visible lines; returns the caret rect if it is on screen.
    fn paint(&mut self, ui: &Ui, doc: &Document, frame: Frame, focused: bool) -> Option<Rect> {
        let rect = frame.rect;
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BACKGROUND);

        let selection = self.selection.range();
        let caret_line = doc.byte_to_line(self.selection.head);
        let mut meshes = GlyphMeshes::default();
        let mut highlights = Vec::new();
        let mut caret = None;
        let mut y = rect.top() - self.anchor.offset;
        let mut line = self.anchor.line;
        while y < rect.bottom() && line < doc.line_count() {
            let range = doc.line_range(line);
            let text = doc.slice(range.clone());
            let height = self.text.line_height(&text);
            self.heights.set_measured(line, height);
            if line == self.anchor.line && self.anchor.offset > height {
                self.anchor.offset = height;
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
                    let r = geometry.caret_rect(self.selection.head - range.start, CARET_WIDTH);
                    caret = Some(r.translate(origin.to_vec2()));
                }
            }
            self.text.draw_line(&mut meshes, &text, origin, TEXT);
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
        self.paint_scrollbar(&painter, frame);
        caret.filter(|c| rect.intersects(*c))
    }

    fn paint_scrollbar(&self, painter: &egui::Painter, frame: Frame) {
        let bar = Rect::from_min_max(pos2(frame.bar_left, frame.rect.top()), frame.rect.max);
        painter.rect_filled(bar, 0.0, SCROLL_TRACK);
        let total = self.heights.total().max(1.0) as f32;
        let viewport = frame.rect.height();
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
        painter.rect_filled(thumb, 3.0, SCROLL_THUMB);
    }
}
