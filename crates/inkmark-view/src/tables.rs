//! GFM tables as source: reading one around an offset, and the edits that
//! re-pad it or change its shape. Every operation re-renders the whole table
//! with its columns padded to line up, and says which cell the caret goes to.
//!
//! The table is read from its source lines (the parse only finds where the
//! table is), so cell text, escaped pipes included, comes back byte for byte.

use std::ops::Range;

use egui::{Key, Modifiers};
use inkmark_buffer::{Document, Edit, EditKind, Selection};
use inkmark_parse::{BlockKind, ParseOutput};
use unicode_width::UnicodeWidthStr;

use crate::commands::EditPlan;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

/// What to do to the table at the caret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableOp {
    /// Re-pad so the pipes line up.
    Format,
    InsertRowAbove,
    InsertRowBelow,
    InsertColumnLeft,
    InsertColumnRight,
    DeleteRow,
    DeleteColumn,
    MoveRowUp,
    MoveRowDown,
    MoveColumnLeft,
    MoveColumnRight,
    Align(Align),
}

/// A table command from the keyboard or the live pane's menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableCommand {
    /// Applies to the table at the caret; outside one, the key does what
    /// it otherwise would.
    Edit(TableOp),
    Insert,
}

/// The table shortcuts (PLAN.md, Table editing): Ctrl+Alt+arrows insert,
/// Ctrl+Alt+(Shift+)Backspace delete, Alt+Shift+arrows move, Ctrl+Alt+F
/// formats, Ctrl+Alt+T inserts a table.
pub(crate) fn shortcut(key: Key, m: Modifiers) -> Option<TableCommand> {
    use TableCommand::{Edit, Insert};
    use TableOp::*;
    let ctrl_alt = m.command && m.alt;
    let alt_shift = m.alt && m.shift && !m.command;
    Some(match key {
        Key::F if ctrl_alt && !m.shift => Edit(Format),
        Key::T if ctrl_alt && !m.shift => Insert,
        Key::ArrowUp if ctrl_alt && !m.shift => Edit(InsertRowAbove),
        Key::ArrowDown if ctrl_alt && !m.shift => Edit(InsertRowBelow),
        Key::ArrowLeft if ctrl_alt && !m.shift => Edit(InsertColumnLeft),
        Key::ArrowRight if ctrl_alt && !m.shift => Edit(InsertColumnRight),
        Key::Backspace if ctrl_alt => Edit(if m.shift { DeleteColumn } else { DeleteRow }),
        Key::ArrowUp if alt_shift => Edit(MoveRowUp),
        Key::ArrowDown if alt_shift => Edit(MoveRowDown),
        Key::ArrowLeft if alt_shift => Edit(MoveColumnLeft),
        Key::ArrowRight if alt_shift => Edit(MoveColumnRight),
        _ => return None,
    })
}

/// Runs `command` at `offset`. `None`: nothing to do (no table there for
/// an edit, or the edit doesn't apply), and `in_table` says whether the
/// key should still be swallowed (inside a table) or do its usual job.
pub(crate) fn run(
    doc: &Document,
    parse: &ParseOutput,
    offset: usize,
    command: TableCommand,
) -> (Option<EditPlan>, bool) {
    match command {
        TableCommand::Insert => (Some(insert_table(doc, offset)), true),
        TableCommand::Edit(op) => {
            let in_table = Table::at(doc, parse, offset).is_some();
            (table_edit(doc, parse, offset, op), in_table)
        }
    }
}

/// The start of the table holding `offset`, if any (to notice the caret
/// leaving it).
pub(crate) fn table_start(doc: &Document, parse: &ParseOutput, offset: usize) -> Option<usize> {
    Table::at(doc, parse, offset).map(|(t, _)| t.range.start)
}

/// The source of the table holding `offset`, if any.
pub(crate) fn table_text(doc: &Document, parse: &ParseOutput, offset: usize) -> Option<String> {
    Table::at(doc, parse, offset).map(|(t, _)| doc.slice(t.range).into_owned())
}

/// A table read from its source.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Table {
    /// Its lines, from the first line's start to the last line's end
    /// (without the final newline).
    pub range: Range<usize>,
    /// Each source line's container prefix (`> `, indentation), delimiter
    /// row included, in source order.
    prefixes: Vec<String>,
    /// Row 0 is the header; the delimiter row isn't a row.
    rows: Vec<Vec<String>>,
    aligns: Vec<Align>,
}

/// A cell: (row, column), row 0 being the header.
pub(crate) type Cell = (usize, usize);

impl Table {
    /// The table holding `offset`, and the cell the offset is in (the
    /// delimiter row counts as the header's).
    pub fn at(doc: &Document, parse: &ParseOutput, offset: usize) -> Option<(Self, Cell)> {
        let block = parse.blocks.iter().find(|b| {
            matches!(b.kind, BlockKind::Table { .. })
                && b.range.start <= offset
                && offset <= b.range.end
        })?;
        let first = doc.byte_to_line(block.range.start);
        let last = doc.byte_to_line(block.range.end.saturating_sub(1).max(block.range.start));
        let range = doc.line_to_byte(first)..doc.line_range(last).end;
        let text = doc.slice(range.clone()).into_owned();
        let table = Self::parse(&text, range.clone())?;
        // The caret's cell.
        let line = doc.byte_to_line(offset.min(range.end)) - first;
        let row = line.saturating_sub(1);
        let line_text = text.lines().nth(line).unwrap_or_default();
        let line_start = text.lines().take(line).map(|l| l.len() + 1).sum::<usize>();
        let within = offset - range.start - line_start;
        let prefix = prefix_len(line_text);
        let col = if within <= prefix {
            0
        } else {
            let content = &line_text[prefix..within.min(line_text.len())];
            let pipes = unescaped_pipes(content);
            let leading = line_text[prefix..].trim_start().starts_with('|');
            pipes.saturating_sub(usize::from(leading))
        };
        let cell = (row.min(table.rows.len() - 1), col.min(table.columns() - 1));
        Some((table, cell))
    }

    fn parse(text: &str, range: Range<usize>) -> Option<Self> {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() < 2 {
            return None;
        }
        let prefixes = lines
            .iter()
            .map(|l| l[..prefix_len(l)].to_owned())
            .collect();
        let cells = |l: &str| split_cells(&l[prefix_len(l)..]);
        let aligns: Vec<Align> = cells(lines[1]).iter().map(|c| parse_align(c)).collect();
        let mut rows = vec![cells(lines[0])];
        rows.extend(lines[2..].iter().map(|l| cells(l)));
        let table = Self {
            range,
            prefixes,
            rows,
            aligns,
        };
        (table.columns() > 0).then_some(table)
    }

    fn text_of(&self, (row, col): Cell) -> Option<String> {
        self.rows.get(row).and_then(|r| r.get(col)).cloned()
    }

    /// The column count: the header's (GFM ignores cells past it).
    fn columns(&self) -> usize {
        self.rows[0].len().max(1)
    }

    /// Applies `op` at `cell`; returns the cell the caret should be in, or
    /// `None` if `op` doesn't apply there (moving the header, deleting the
    /// only column).
    fn apply(&mut self, op: TableOp, (row, col): Cell) -> Option<Cell> {
        let cols = self.columns();
        match op {
            TableOp::Format => Some((row, col)),
            TableOp::InsertRowAbove if row == 0 => None,
            TableOp::InsertRowAbove | TableOp::InsertRowBelow => {
                let at = if op == TableOp::InsertRowAbove {
                    row
                } else {
                    row + 1
                };
                self.rows.insert(at, vec![String::new(); cols]);
                let prefix = self.prefixes.last().cloned().unwrap_or_default();
                self.prefixes.push(prefix);
                Some((at, col))
            }
            TableOp::InsertColumnLeft | TableOp::InsertColumnRight => {
                let at = if op == TableOp::InsertColumnLeft {
                    col
                } else {
                    col + 1
                };
                for r in &mut self.rows {
                    if r.len() < cols {
                        r.resize(cols, String::new());
                    }
                    r.insert(at, String::new());
                }
                self.aligns.resize(cols, Align::None);
                self.aligns.insert(at, Align::None);
                Some((row, at))
            }
            TableOp::DeleteRow if row == 0 => None,
            TableOp::DeleteRow => {
                self.rows.remove(row);
                self.prefixes.pop();
                Some((row.min(self.rows.len() - 1), col))
            }
            TableOp::DeleteColumn if cols == 1 => None,
            TableOp::DeleteColumn => {
                for r in &mut self.rows {
                    if col < r.len() {
                        r.remove(col);
                    }
                }
                if col < self.aligns.len() {
                    self.aligns.remove(col);
                }
                Some((row, col.min(cols - 2)))
            }
            TableOp::MoveRowUp if row <= 1 => None,
            TableOp::MoveRowUp => {
                self.rows.swap(row, row - 1);
                Some((row - 1, col))
            }
            TableOp::MoveRowDown if row == 0 || row + 1 >= self.rows.len() => None,
            TableOp::MoveRowDown => {
                self.rows.swap(row, row + 1);
                Some((row + 1, col))
            }
            TableOp::MoveColumnLeft if col == 0 => None,
            TableOp::MoveColumnRight if col + 1 >= cols => None,
            TableOp::MoveColumnLeft | TableOp::MoveColumnRight => {
                let other = if op == TableOp::MoveColumnLeft {
                    col - 1
                } else {
                    col + 1
                };
                for r in &mut self.rows {
                    if r.len() < cols {
                        r.resize(cols, String::new());
                    }
                    r.swap(col, other);
                }
                self.aligns.resize(cols, Align::None);
                self.aligns.swap(col, other);
                Some((row, other))
            }
            TableOp::Align(align) => {
                self.aligns.resize(cols, Align::None);
                self.aligns[col] = align;
                Some((row, col))
            }
        }
    }

    /// The table as aligned Markdown, and where each cell's text starts
    /// (offsets into the text, by row and column).
    fn render(&self) -> (String, Vec<Vec<usize>>) {
        let cols = self.columns();
        let width = |c: usize| {
            self.rows
                .iter()
                .filter_map(|r| r.get(c))
                .map(|s| s.width())
                .max()
                .unwrap_or(0)
                .max(3)
        };
        let widths: Vec<usize> = (0..cols).map(width).collect();
        let align = |c: usize| self.aligns.get(c).copied().unwrap_or(Align::None);
        let mut out = String::new();
        let mut starts = Vec::new();
        let prefix = |line: usize| {
            self.prefixes
                .get(line)
                .or(self.prefixes.last())
                .cloned()
                .unwrap_or_default()
        };
        let row_line = |row: usize| if row == 0 { 0 } else { row + 1 };
        for (r, cells) in self.rows.iter().enumerate() {
            if r == 1 {
                out.push_str(&prefix(1));
                out.push('|');
                for (c, &w) in widths.iter().enumerate() {
                    out.push(' ');
                    out.push_str(&delimiter(align(c), w));
                    out.push_str(" |");
                }
                out.push('\n');
            }
            out.push_str(&prefix(row_line(r)));
            out.push('|');
            let mut row_starts = Vec::new();
            for c in 0..cells.len().max(cols) {
                let text = cells.get(c).map(String::as_str).unwrap_or("");
                let w = widths.get(c).copied().unwrap_or_else(|| text.width());
                let pad = w.saturating_sub(text.width());
                let (before, after) = match align(c) {
                    Align::Right => (pad, 0),
                    Align::Center => (pad / 2, pad - pad / 2),
                    Align::None | Align::Left => (0, pad),
                };
                out.push(' ');
                out.push_str(&" ".repeat(before));
                row_starts.push(out.len());
                out.push_str(text);
                out.push_str(&" ".repeat(after));
                out.push_str(" |");
            }
            starts.push(row_starts);
            out.push('\n');
        }
        // A table of just a header still has its delimiter row.
        if self.rows.len() == 1 {
            out.push_str(&prefix(1));
            out.push('|');
            for (c, &w) in widths.iter().enumerate() {
                out.push(' ');
                out.push_str(&delimiter(align(c), w));
                out.push_str(" |");
            }
            out.push('\n');
        }
        out.pop();
        (out, starts)
    }
}

/// The edits for `op` on the table at `offset`, with the caret in the cell
/// it lands in, at the same place in the cell's text when it stays there.
/// `None` outside a table, when `op` doesn't apply, or when nothing would
/// change.
pub(crate) fn table_edit(
    doc: &Document,
    parse: &ParseOutput,
    offset: usize,
    op: TableOp,
) -> Option<EditPlan> {
    let (mut table, cell) = Table::at(doc, parse, offset)?;
    let in_cell = cell_offset(doc, &table, cell, offset);
    let before = table.text_of(cell);
    let target = table.apply(op, cell)?;
    let (text, starts) = table.render();
    if text == doc.slice(table.range.clone()) {
        return None;
    }
    // Same text in the target cell (formatted, moved with its row or
    // column): the caret keeps its place in it. Otherwise its start.
    let keep = if table.text_of(target) == before {
        in_cell
    } else {
        0
    };
    let start = starts
        .get(target.0)
        .and_then(|r| r.get(target.1))
        .copied()
        .unwrap_or(0);
    let cell_len = table
        .rows
        .get(target.0)
        .and_then(|r| r.get(target.1))
        .map_or(0, String::len);
    let caret = table.range.start + start + keep.min(cell_len);
    Some(EditPlan {
        edits: vec![Edit::replace(table.range.clone(), text)],
        selection: Selection::caret(caret),
        kind: EditKind::Other,
    })
}

/// Re-pads the table starting at `start` (the one the caret just left), if
/// it's still there and needs it. Returns the edit and how much longer the
/// table got, to move a caret after it.
pub(crate) fn format_table(
    doc: &Document,
    parse: &ParseOutput,
    start: usize,
) -> Option<(Edit, isize)> {
    let (table, _) = Table::at(doc, parse, start)?;
    if table.range.start != start {
        return None;
    }
    let (text, _) = table.render();
    if text == doc.slice(table.range.clone()) {
        return None;
    }
    let grew = text.len() as isize - table.range.len() as isize;
    Some((Edit::replace(table.range, text), grew))
}

/// A new 3×3 table (a header and two rows) on its own after the caret's
/// line, with its first header cell selected.
pub(crate) fn insert_table(doc: &Document, offset: usize) -> EditPlan {
    const HEADER: &str = "Column 1";
    let line = doc.byte_to_line(offset);
    let range = doc.line_range(line);
    let empty = doc.slice(range.clone()).trim().is_empty();
    let table = format!(
        "| {HEADER} | Column 2 | Column 3 |\n| -------- | -------- | -------- |\n|          |          |          |\n|          |          |          |"
    );
    // On an empty line: there, after a blank line if the line before has
    // text (a table can't start inside a paragraph). Otherwise after this
    // line, as its own block.
    let prev_text = line > 0 && !doc.slice(doc.line_range(line - 1)).trim().is_empty();
    let (at, before) = match (empty, prev_text) {
        (true, false) => (range.start, String::new()),
        (true, true) => (range.start, "\n".to_owned()),
        (false, _) => (range.end, "\n\n".to_owned()),
    };
    let next_blank =
        doc.line_count() <= line + 1 || doc.slice(doc.line_range(line + 1)).trim().is_empty();
    let after = if empty && next_blank { "" } else { "\n" };
    let replaced = if empty { range.clone() } else { at..at };
    let text = format!("{before}{table}{after}");
    let start = at + before.len() + 2;
    EditPlan {
        edits: vec![Edit::replace(replaced, text)],
        selection: Selection {
            anchor: start,
            head: start + HEADER.len(),
        },
        kind: EditKind::Other,
    }
}

/// How far into `cell`'s text `offset` is (0 if it's in the padding or
/// another cell).
fn cell_offset(doc: &Document, table: &Table, (row, col): Cell, offset: usize) -> usize {
    let line = doc.byte_to_line(table.range.start) + if row == 0 { 0 } else { row + 1 };
    if doc.byte_to_line(offset) != line {
        return 0;
    }
    let Some(text) = table.rows.get(row).and_then(|r| r.get(col)) else {
        return 0;
    };
    // Find this cell's text on its source line, then the offset within it.
    let line_range = doc.line_range(line);
    let source = doc.slice(line_range.clone()).into_owned();
    let prefix = prefix_len(&source);
    let Some(span) = cell_spans(&source[prefix..]).into_iter().nth(col) else {
        return 0;
    };
    let raw = &source[prefix + span.start..prefix + span.end];
    let lead = raw.len() - raw.trim_start().len();
    let text_start = line_range.start + prefix + span.start + lead;
    offset.saturating_sub(text_start).min(text.len())
}

/// `> `, `>> `, indentation: the container markup before a table line.
fn prefix_len(line: &str) -> usize {
    let mut i = 0;
    let bytes = line.as_bytes();
    loop {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'>' {
            i += 1;
            continue;
        }
        return i;
    }
}

/// Unescaped `|` in `s`.
fn unescaped_pipes(s: &str) -> usize {
    let mut count = 0;
    let mut escaped = false;
    for c in s.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '|' => count += 1,
            _ => {}
        }
    }
    count
}

/// Byte ranges of the cells in a row's content (after its prefix), between
/// unescaped pipes, without the optional leading and trailing pipe.
fn cell_spans(content: &str) -> Vec<Range<usize>> {
    let mut bounds = vec![];
    let mut escaped = false;
    for (i, c) in content.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '|' => bounds.push(i),
            _ => {}
        }
    }
    let trimmed_start = content.len() - content.trim_start().len();
    let trimmed_end = content.trim_end().len();
    let mut start = 0;
    let mut spans = Vec::new();
    for &b in &bounds {
        if b == trimmed_start && start == 0 {
            start = b + 1;
            continue;
        }
        spans.push(start..b);
        start = b + 1;
    }
    let last_is_pipe = bounds.last().is_some_and(|&b| b + 1 == trimmed_end);
    if !last_is_pipe {
        spans.push(start..content.len());
    }
    spans
}

fn split_cells(content: &str) -> Vec<String> {
    cell_spans(content)
        .into_iter()
        .map(|r| content[r].trim().to_owned())
        .collect()
}

fn parse_align(cell: &str) -> Align {
    match (cell.starts_with(':'), cell.ends_with(':') && cell.len() > 1) {
        (true, true) => Align::Center,
        (true, false) => Align::Left,
        (false, true) => Align::Right,
        (false, false) => Align::None,
    }
}

/// A delimiter cell `w` wide for `align`.
fn delimiter(align: Align, w: usize) -> String {
    match align {
        Align::None => "-".repeat(w),
        Align::Left => format!(":{}", "-".repeat(w - 1)),
        Align::Right => format!("{}:", "-".repeat(w - 1)),
        Align::Center => format!(":{}:", "-".repeat(w - 2)),
    }
}

#[cfg(test)]
mod tests {
    use inkmark_parse::{GfmParser, MarkdownParser};

    use super::*;

    /// Runs `op` with the caret just after `at` in `src`; returns the new
    /// source and the text from the caret to the end of its line.
    fn run(src: &str, at: &str, op: TableOp) -> Option<(String, String)> {
        let doc = Document::from_text(src);
        let parse = GfmParser.parse(src);
        let offset = src.find(at).unwrap() + at.len();
        let plan = table_edit(&doc, &parse, offset, op)?;
        let mut out = src.to_owned();
        for e in plan.edits.iter().rev() {
            out.replace_range(e.range.clone(), &e.insert);
        }
        let caret = plan.selection.head;
        let rest = out[caret..].lines().next().unwrap_or("").to_owned();
        Some((out, rest))
    }

    /// The cells of every row as GFM reads them, by parsing the source.
    fn cells(src: &str) -> Vec<Vec<String>> {
        let parse = GfmParser.parse(src);
        let table = parse
            .blocks
            .iter()
            .find(|b| matches!(b.kind, BlockKind::Table { .. }))
            .expect("still a table");
        parse
            .blocks
            .table_rows(&table)
            .into_iter()
            .map(|(_, cells)| {
                cells
                    .into_iter()
                    .map(|c| src[c.range].trim().to_owned())
                    .collect()
            })
            .collect()
    }

    const T: &str = "| a | bbb |\n|:-|--:|\n| cc | d |\n| e | f |\n";

    #[test]
    fn formatting_lines_up_the_pipes_and_keeps_the_text() {
        let (out, rest) = run(T, "| c", TableOp::Format).unwrap();
        assert_eq!(
            out,
            "| a   | bbb |\n| :-- | --: |\n| cc  |   d |\n| e   |   f |\n"
        );
        assert_eq!(cells(&out), cells(T));
        assert!(rest.starts_with('c'), "caret stays in its cell: {rest:?}");
        // Already formatted: nothing to do.
        assert!(run(&out, "| c", TableOp::Format).is_none());
    }

    #[test]
    fn wide_text_escapes_and_containers_survive_formatting() {
        let src = "> | 名前 | x |\n> |---|---|\n> | a\\|b | `c|` |\n";
        let (out, _) = run(src, "名", TableOp::Format).unwrap();
        // GFM splits a cell at an unescaped pipe even in a code span, so
        // `` `c|` `` is two cells, the second past the header's count.
        assert_eq!(
            out,
            "> | 名前 | x   |\n> | ---- | --- |\n> | a\\|b | `c  | ` |\n"
        );
        assert_eq!(cells(&out), cells(src));
    }

    #[test]
    fn rows_insert_delete_and_move() {
        let (out, rest) = run(T, "| cc", TableOp::InsertRowBelow).unwrap();
        assert_eq!(cells(&out).len(), 4, "header, cc, the new row, e");
        assert_eq!(cells(&out)[2], vec!["", ""], "a new empty row after cc's");
        assert!(rest.starts_with("    |"), "caret in the new row: {rest:?}");
        let (out, _) = run(T, "| cc", TableOp::InsertRowAbove).unwrap();
        assert_eq!(cells(&out)[1], vec!["", ""]);
        assert!(
            run(T, "| a", TableOp::InsertRowAbove).is_none(),
            "nothing above the header"
        );

        let (out, rest) = run(T, "| cc", TableOp::DeleteRow).unwrap();
        assert_eq!(cells(&out), vec![vec!["a", "bbb"], vec!["e", "f"]]);
        assert!(rest.starts_with('e'), "{rest:?}");
        assert!(
            run(T, "| a", TableOp::DeleteRow).is_none(),
            "the header stays"
        );

        let (out, rest) = run(T, "| e", TableOp::MoveRowUp).unwrap();
        assert_eq!(cells(&out)[1], vec!["e", "f"]);
        assert!(rest.starts_with("   |"), "{rest:?}");
        assert!(
            run(T, "| cc", TableOp::MoveRowUp).is_none(),
            "not above the header"
        );
        let (out, _) = run(T, "| cc", TableOp::MoveRowDown).unwrap();
        assert_eq!(cells(&out)[2], vec!["cc", "d"]);
        assert!(run(T, "| e", TableOp::MoveRowDown).is_none());
    }

    #[test]
    fn columns_insert_delete_move_and_align() {
        let (out, _) = run(T, "| bbb", TableOp::InsertColumnRight).unwrap();
        assert_eq!(cells(&out)[0], vec!["a", "bbb", ""]);
        assert_eq!(cells(&out)[1], vec!["cc", "d", ""]);
        let (out, _) = run(T, "| a", TableOp::InsertColumnLeft).unwrap();
        assert_eq!(cells(&out)[0], vec!["", "a", "bbb"]);

        let (out, rest) = run(T, "| a", TableOp::DeleteColumn).unwrap();
        assert_eq!(cells(&out), vec![vec!["bbb"], vec!["d"], vec!["f"]]);
        assert!(rest.starts_with("bbb"), "{rest:?}");
        assert!(
            run(&out, "| b", TableOp::DeleteColumn).is_none(),
            "the last column stays"
        );

        let (out, rest) = run(T, "| ", TableOp::MoveColumnRight).unwrap();
        assert_eq!(cells(&out)[0], vec!["bbb", "a"]);
        assert!(
            out.contains("| --: | :-- |"),
            "alignments move with their column:\n{out}"
        );
        assert!(
            rest.starts_with('a'),
            "the caret moves with the cell: {rest:?}"
        );
        assert!(run(T, "| a", TableOp::MoveColumnLeft).is_none());

        let (out, _) = run(T, "| a", TableOp::Align(Align::Center)).unwrap();
        assert!(out.contains("| :-: |"), "{out}");
        let (out, _) = run(T, "| bbb", TableOp::Align(Align::None)).unwrap();
        assert!(out.lines().nth(1).unwrap().ends_with("| --- |"), "{out}");
    }

    #[test]
    fn short_and_long_rows_keep_their_cells() {
        // A short row is padded out; cells past the header's count are kept
        // (GFM ignores them, but they're the user's text).
        let src = "| a | b |\n|---|---|\n| x |\n| 1 | 2 | 3 |\n";
        let (out, _) = run(src, "| x", TableOp::Format).unwrap();
        assert_eq!(
            out,
            "| a   | b   |\n| --- | --- |\n| x   |     |\n| 1   | 2   | 3 |\n"
        );
    }

    #[test]
    fn a_new_table_goes_on_its_own_with_the_first_header_selected() {
        let src = "Intro.\nMore.\n";
        let doc = Document::from_text(src);
        let plan = insert_table(&doc, 2);
        let mut out = src.to_owned();
        out.replace_range(plan.edits[0].range.clone(), &plan.edits[0].insert);
        assert!(out.starts_with("Intro.\n\n| Column 1 |"), "{out}");
        assert!(out.contains("|\n\nMore."), "{out}");
        assert_eq!(&out[plan.selection.range()], "Column 1");
        assert_eq!(cells(&out).len(), 3);
        // On an empty line, in place.
        let src = "Intro.\n\n\nEnd.\n";
        let doc = Document::from_text(src);
        let plan = insert_table(&doc, src.find("\n\n").unwrap() + 1);
        let mut out = src.to_owned();
        out.replace_range(plan.edits[0].range.clone(), &plan.edits[0].insert);
        assert!(out.starts_with("Intro.\n\n| Column 1 |"), "{out}");
        assert!(out.ends_with("|\n\nEnd.\n"), "{out}");
    }
}
