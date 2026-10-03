//! The raw Markdown pane: a virtualized, soft-wrapping editor over a
//! [`Document`], drawn with [`TextRenderer`].

use std::borrow::Cow;
use std::ops::Range;
use std::time::{Duration, Instant};

use egui::output::IMEOutput;
use egui::{
    Align2, CursorIcon, Event, EventFilter, FontId, IMEPurpose, Id, ImeEvent, Key, Modifiers, Pos2,
    Rect, Response, Sense, Ui, pos2, vec2,
};
use inkmark_buffer::{Bias, Change, Document, Edit, EditKind, Selection};
use inkmark_parse::{ParseOutput, ParseState};
use inkmark_text::{GlyphMeshes, ScrollAnchor, SharedFonts, TextConfig, TextRenderer};

use crate::commands::{self, EditPlan};
use crate::folds::Folds;
use crate::keys::{self, Action};
use crate::lines::SCROLLBAR_WIDTH;
use crate::lines::{LineIndex, ScrollPos, Synced};
use crate::motion;
use crate::tables;
use crate::theme::{self, Theme};

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
    /// Colors, refreshed from the context every frame.
    theme: std::sync::Arc<Theme>,
    /// The caret at this offset is at the end of a row that wrapped
    /// mid-word (End, or a click past the row), not the start of the next
    /// row. Stale as soon as the caret is anywhere else.
    upstream_at: Option<usize>,
    /// Whether the last `offset_at` hit was such a row end.
    hit_upstream: bool,
    pub font_size: f32,
    pub line_height: f32,
    /// Shortcuts, shared with the other pane and the app shell.
    keys: keys::KeyMap,
    /// Heading bodies hidden in this pane. Source ranges, so an edit moves
    /// them and a reparse leaves them where the bytes are.
    folds: Folds,
    /// `set_selection` landed somewhere that may be inside a fold.
    pending_reveal: bool,
    /// Folds changed after this frame's first sync and need heights again.
    fold_resync: bool,
    /// Gutter hits painted last frame, each the body it toggles.
    marker_hits: Vec<MarkerHit>,
    /// Heading markers for the parse `region_revision` was built from.
    /// Kept and shifted when a frame's parse is only a placeholder, so the
    /// marks do not vanish between keystrokes.
    regions: Vec<CachedRegion>,
    region_revision: Option<u64>,
}

/// A heading the code pane can fold, remembered across frames.
struct CachedRegion {
    /// First byte of the heading line.
    at: usize,
    line: usize,
    body: Range<usize>,
}

struct MarkerHit {
    rect: Rect,
    body: Range<usize>,
    /// Nested heading body start inside `body`, when there is one.
    child: Option<usize>,
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
            theme: std::sync::Arc::new(Theme::dark()),
            upstream_at: None,
            hit_upstream: false,
            font_size: 14.0,
            line_height: 21.0,
            keys: keys::KeyMap::builtin(),
            folds: Folds::default(),
            pending_reveal: false,
            fold_resync: false,
            marker_hits: Vec::new(),
            regions: Vec::new(),
            region_revision: None,
        }
    }

    /// Replaces the shortcuts. The app does this when config.toml changes.
    pub fn set_keys(&mut self, keys: keys::KeyMap) {
        self.keys = keys;
    }

    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// Source ranges the code pane is currently hiding.
    pub fn hidden_ranges(&self) -> &[Range<usize>] {
        self.folds.ranges()
    }

    /// The height currently stored for `line` (0 while that line is folded).
    pub fn measured_height(&self, line: usize) -> f32 {
        if line < self.lines.heights.len() {
            self.lines.heights.height(line)
        } else {
            0.0
        }
    }

    /// Moves the selection (e.g. from search or the other pane) and scrolls to it.
    pub fn set_selection(&mut self, selection: Selection) {
        self.selection = selection;
        self.selection_current = true;
        self.preferred_x = None;
        self.reveal_caret = REVEAL_FRAMES;
        self.pending_reveal = true;
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
        self.folds.clear();
        self.pending_reveal = false;
        self.fold_resync = false;
        self.marker_hits.clear();
        self.regions.clear();
        self.region_revision = None;
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
        mut parse: Option<&mut ParseState>,
    ) -> Response {
        self.theme = theme::current(ui.ctx());
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
        // Rebase first. A jump's offsets are already in the current
        // document, and the folds may still be in the previous one (the
        // code pane was hidden while the live pane was edited).
        self.sync(doc, true);
        if self.pending_reveal {
            self.pending_reveal = false;
            if self.folds.reveal(doc, self.selection.head)
                || self.folds.reveal(doc, self.selection.anchor)
            {
                self.lines.invalidate();
                self.fold_resync = true;
            }
        }
        if let Some(pos) = self.pending_scroll.take() {
            self.apply_scroll_pos(pos);
        }

        if focused {
            let current = parse
                .as_deref()
                .map(ParseState::output)
                .filter(|p| p.map.len() == doc.len());
            self.handle_events(ui, doc, viewport, current);
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
        if self.fold_resync {
            self.fold_resync = false;
            self.sync(doc, false);
        }
        if self.reveal_caret > 0 {
            self.scroll_caret_into_view(ui, doc, viewport);
        }
        if let Some(parse) = parse.as_mut() {
            if let Some(wait) = parse.update(doc) {
                ui.ctx().request_repaint_after(wait);
            }
            self.refresh_regions(doc, parse);
        }
        let parse = parse.map(|p| &*p);
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
        let synced = self.lines.sync(doc, |chars| text.estimate_height(chars));
        if let Synced::Changed(changes) = &synced {
            self.folds.rebase(changes);
            self.folds.align_lines(doc);
            self.rebase_regions(doc, changes);
            if map_selection {
                for c in changes {
                    self.selection.anchor = c.map(self.selection.anchor, Bias::Left);
                    self.selection.head = c.map(self.selection.head, Bias::Left);
                }
            }
        }
        self.clamp_selection(doc);
        if !matches!(synced, Synced::Unchanged) {
            self.apply_fold_heights(doc);
        }
    }

    /// Rebuilds heading markers when the parse revision changes. A
    /// placeholder parse (no blocks yet) keeps the markers already shifted
    /// through the edit, so they stay clickable while a full parse runs.
    fn refresh_regions(&mut self, doc: &Document, parse: &ParseState) {
        let revision = parse.revision();
        if self.region_revision == Some(revision) {
            return;
        }
        let output = parse.output();
        if output.map.len() != doc.len() {
            return;
        }
        if output.blocks.iter().next().is_none() && !doc.is_empty() {
            return;
        }
        self.regions = inkmark_parse::heading_regions(doc, output)
            .into_iter()
            .filter(|region| region.line < doc.line_count())
            .map(|region| CachedRegion {
                at: doc.line_to_byte(region.line),
                line: region.line,
                body: region.body,
            })
            .collect();
        self.region_revision = Some(revision);
    }

    /// Moves cached markers through `changes`, then onto whole lines.
    fn rebase_regions(&mut self, doc: &Document, changes: &[Change]) {
        for region in &mut self.regions {
            for change in changes {
                region.at = change.map(region.at, Bias::Left);
                let start = change.map(region.body.start, Bias::Right);
                let end = change.map(region.body.end, Bias::Left).max(start);
                region.body = start..end;
            }
            let len = doc.len();
            region.at = region.at.min(len);
            if len > 0 {
                region.at = doc.line_to_byte(doc.byte_to_line(region.at));
            }
            region.body = crate::folds::line_aligned(doc, region.body.start, region.body.end);
            region.line = if doc.line_count() == 0 {
                0
            } else {
                doc.byte_to_line(region.at.min(len))
            };
        }
        self.regions
            .retain(|region| !region.body.is_empty() && region.line < doc.line_count());
    }

    /// The heading on `line`, and where a nested heading's body starts.
    fn marker_for(&self, line: usize) -> Option<(Range<usize>, Option<usize>)> {
        let index = self.regions.partition_point(|region| region.line < line);
        let (start, end) = {
            let region = self.regions.get(index)?;
            if region.line != line || region.body.is_empty() {
                return None;
            }
            (region.body.start, region.body.end)
        };
        let child = self.regions[index + 1..]
            .iter()
            .find(|next| next.body.start > start && next.body.start < end)
            .map(|next| next.body.start);
        Some((start..end, child))
    }

    /// Gives every hidden line a height of 0 so scroll math skips it.
    /// Visible lines keep the estimate or measurement they already have.
    fn apply_fold_heights(&mut self, doc: &Document) {
        let ranges: Vec<Range<usize>> = self.folds.ranges().to_vec();
        let len = self.lines.heights.len();
        for body in ranges {
            let start = body.start.min(doc.len());
            if doc.line_count() == 0 || len == 0 {
                continue;
            }
            let mut line = doc.byte_to_line(start);
            if doc.line_to_byte(line) < body.start {
                line += 1;
            }
            while line < len && line < doc.line_count() && doc.line_to_byte(line) < body.end {
                self.lines.heights.set_measured(line, 0.0);
                line += 1;
            }
        }
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
        // A jump that lands in a folded body opens it, so the caret is visible.
        if self.folds.reveal(doc, self.selection.head)
            || self.folds.reveal(doc, self.selection.anchor)
        {
            self.lines.invalidate();
            self.fold_resync = true;
        }
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
                    let mut prev = line - 1;
                    while prev > 0 && self.folds.hides_line(doc, prev) {
                        prev -= 1;
                    }
                    // Hidden lines above, and nothing visible: stay here.
                    if self.folds.hides_line(doc, prev) {
                        break;
                    }
                    line = prev;
                    geometry = self.text.geometry(&Self::line_text(doc, line));
                    row = geometry.rows.len() - 1;
                } else {
                    head = 0;
                    break;
                }
            } else if row + 1 < geometry.rows.len() {
                row += 1;
            } else if line + 1 < doc.line_count() {
                let mut next = line + 1;
                while next < doc.line_count() && self.folds.hides_line(doc, next) {
                    next += 1;
                }
                // The rest of the document is folded. Stay on this line
                // instead of putting the caret in text that is not drawn.
                if next >= doc.line_count() {
                    break;
                }
                line = next;
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

    fn handle_events(
        &mut self,
        ui: &Ui,
        doc: &mut Document,
        viewport: f32,
        parse: Option<&ParseOutput>,
    ) {
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
                } if self.preedit.is_empty() => {
                    self.handle_key(doc, key, modifiers, viewport, parse)
                }
                _ => {}
            }
        }
    }

    fn handle_key(
        &mut self,
        doc: &mut Document,
        key: Key,
        modifiers: Modifiers,
        viewport: f32,
        parse: Option<&ParseOutput>,
    ) {
        let shift = modifiers.shift;
        let mut from_table = false;
        if let Some((action, extend)) = self.keys.editor_gesture(key, modifiers)
            && self.run_editor_action(doc, parse, action, extend, &mut from_table)
        {
            return;
        }
        // A table chord that doesn't apply (no table here) keeps the key's
        // ordinary meaning, so Ctrl+Alt+Left still moves by word. An unbound
        // Ctrl chord does not: word motion is itself a binding.
        let word = from_table && modifiers.command;
        let range = self.selection.range();
        let head = self.selection.head;
        match key {
            Key::ArrowLeft if !shift && !range.is_empty() => self.move_to(doc, range.start, false),
            Key::ArrowRight if !shift && !range.is_empty() => self.move_to(doc, range.end, false),
            Key::ArrowLeft => {
                let target = if word {
                    motion::prev_word(doc, head)
                } else {
                    motion::prev_grapheme(doc, head)
                };
                self.move_to(doc, target, shift);
            }
            Key::ArrowRight => {
                let target = if word {
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
                let start = if word {
                    motion::prev_word(doc, head)
                } else {
                    motion::prev_grapheme(doc, head)
                };
                self.delete(doc, start..head, EditKind::Deleting);
            }
            Key::Delete => {
                let end = if word {
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
            Key::Escape if !range.is_empty() => self.move_to(doc, head, false),
            _ => {}
        }
    }

    /// Runs a chord from the key table. False when a table action doesn't
    /// apply, so the key falls through; `from_table` says that's why.
    fn run_editor_action(
        &mut self,
        doc: &mut Document,
        parse: Option<&ParseOutput>,
        action: Action,
        extend: bool,
        from_table: &mut bool,
    ) -> bool {
        if let Some(command) = tables::command(action) {
            let Some(parse) = parse else {
                *from_table = true;
                return false;
            };
            match tables::run(doc, parse, self.selection.head, command) {
                (Some(plan), _) => {
                    self.apply_plan(doc, plan);
                    return true;
                }
                (None, true) => return true,
                (None, false) => {
                    *from_table = true;
                    return false;
                }
            }
        }
        let sel = self.selection;
        let range = sel.range();
        let head = sel.head;
        if let Some(level) = action.heading_level() {
            self.apply_plan(doc, commands::set_heading(doc, sel, level));
            return true;
        }
        match action {
            Action::Undo => self.undo(doc),
            Action::Redo => self.redo(doc),
            Action::Bold => self.apply_plan(doc, commands::toggle_wrap(doc, sel, "**", &["__"])),
            Action::Italic => self.apply_plan(doc, commands::toggle_wrap(doc, sel, "*", &["_"])),
            Action::Code => self.apply_plan(doc, commands::toggle_wrap(doc, sel, "`", &[])),
            Action::Strikethrough => {
                self.apply_plan(doc, commands::toggle_wrap(doc, sel, "~~", &["~"]))
            }
            Action::Link => self.apply_plan(doc, commands::insert_link(doc, sel)),
            Action::ToggleTask => self.apply_plan(doc, commands::toggle_task(doc, sel)),
            Action::SelectAll => {
                self.selection = Selection {
                    anchor: 0,
                    head: doc.len(),
                };
                doc.seal_undo_step();
            }
            // Ctrl+Left with a selection collapses it, same as Left. The
            // word jump waits until the caret is alone.
            Action::WordLeft | Action::WordRight if !extend && !range.is_empty() => {
                let at = if action == Action::WordLeft {
                    range.start
                } else {
                    range.end
                };
                self.move_to(doc, at, false);
            }
            Action::WordLeft => self.move_to(doc, motion::prev_word(doc, head), extend),
            Action::WordRight => self.move_to(doc, motion::next_word(doc, head), extend),
            Action::DeleteWordLeft | Action::DeleteWordRight if !range.is_empty() => {
                self.delete(doc, range, EditKind::Deleting);
            }
            Action::DeleteWordLeft => {
                self.delete(doc, motion::prev_word(doc, head)..head, EditKind::Deleting);
            }
            Action::DeleteWordRight => {
                self.delete(doc, head..motion::next_word(doc, head), EditKind::Deleting);
            }
            Action::DocumentStart => self.move_to(doc, 0, extend),
            Action::DocumentEnd => self.move_to(doc, doc.len(), extend),
            _ => return false,
        }
        true
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
            let hit = self
                .marker_hits
                .iter()
                .find(|hit| hit.rect.contains(pos))
                .map(|hit| (hit.body.clone(), hit.child));
            if let Some((body, child)) = hit {
                self.folds.toggle(body, child);
                self.lines.invalidate();
                self.fold_resync = true;
                self.drag = None;
                return;
            }
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
        if self.folds.hides_line(doc, line) {
            self.reveal_caret = 0;
            return;
        }
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
        painter.rect_filled(rect, 0.0, self.theme.background);

        let selection = self.selection.range();
        let caret_line = doc.byte_to_line(self.selection.head);
        let mut meshes = GlyphMeshes::default();
        let mut highlights = Vec::new();
        let mut colors = Vec::new();
        let mut caret = None;
        let mut y = rect.top() - self.lines.anchor.offset;
        let mut line = self.lines.anchor.line;
        self.marker_hits.clear();
        while y < rect.bottom() && line < doc.line_count() {
            if let Some(next) = self.folds.hidden_until(doc, line) {
                line = next;
                continue;
            }
            let range = doc.line_range(line);
            let text = doc.slice(range.clone());
            let height = self.text.line_height(&text);
            self.lines.heights.set_measured(line, height);
            if let Some((body, child)) = self.marker_for(line) {
                let mark = if self.folds.covers(&body, child) {
                    "▸"
                } else {
                    "▾"
                };
                painter.text(
                    pos2(rect.left() + 1.0, y + height * 0.5),
                    Align2::LEFT_CENTER,
                    mark,
                    FontId::proportional(12.0),
                    self.theme.text,
                );
                self.marker_hits.push(MarkerHit {
                    rect: Rect::from_min_size(pos2(rect.left(), y), vec2(PADDING, height)),
                    body,
                    child,
                });
            }
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
                    if let Some(color) = self.theme.code_color(&span)
                        && !local.is_empty()
                    {
                        colors.push((local, color));
                    }
                }
            }
            self.text
                .draw_line_colored(&mut meshes, &text, origin, self.theme.text, &colors);
            y += height;
            line += 1;
        }
        for r in highlights {
            painter.rect_filled(r, 0.0, self.theme.selection);
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
            painter.rect_filled(bg, 0.0, self.theme.background);
            self.text
                .draw_line(&mut meshes, &preedit, c.min, self.theme.text);
            painter.hline(bg.x_range(), bg.bottom() - 1.0, (1.0, self.theme.caret));
        }
        self.text.end_frame(meshes, &painter);
        if focused && let Some(c) = caret {
            painter.rect_filled(c, 0.0, self.theme.caret);
        }
        self.lines.paint_scrollbar(
            &painter,
            Rect::from_min_max(pos2(frame.bar_left, rect.top()), rect.max),
            &self.theme,
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
        m.paint_background(painter, self.theme.mini_background);
        let parse = parse.filter(|p| p.map.len() == doc.len());
        let (top, bottom) = m.window();
        let mut line = heights.line_at(top).line;
        let mut y = heights.offset_of(line);
        while y < bottom && line < doc.line_count().min(heights.len()) {
            let h = f64::from(heights.height(line));
            if h == 0.0 {
                line += 1;
                continue;
            }
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
                    .and_then(|s| self.theme.code_color(&s))
                    .map_or(self.theme.mini_text, |c| c.gamma_multiply(0.55));
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
        let fill = if ui.rect_contains_pointer(rect) {
            self.theme.mini_viewport_hover
        } else {
            self.theme.mini_viewport
        };
        m.paint_viewport(painter, fill, self.theme.mini_viewport_edge);
    }
}
