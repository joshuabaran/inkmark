//! Very long source lines (PLAN.md §1, CRO-112). Shaping a line costs time
//! in proportion to its length, and a 1 MB line took over a second on every
//! keystroke. Over [`LONG_LINE`] bytes:
//!
//! - The code pane wraps the line itself, on its monospace grid, without
//!   shaping it. Every character takes one cell (two for wide ones, a tab
//!   to the next stop), as the renderer snaps glyphs, so the rows, their
//!   height and every caret position follow from counting cells. Only the
//!   rows on screen are shaped, each as a short line of its own.
//! - The live pane shows a block holding such a line as a one-line notice
//!   (see `live_layout::build`); it's edited in the code pane.

use std::ops::Range;

use inkmark_text::{ClusterSpan, LineGeometry, Row};
use unicode_width::UnicodeWidthChar;

/// Lines longer than this (in bytes) take the long-line path.
pub(crate) const LONG_LINE: usize = 64 * 1024;

/// Tab stops every this many cells, as the renderer lays tabs out.
const TAB_WIDTH: usize = 4;

/// Cells `c` takes at column `col`. Combining marks take none and join the
/// character before; a control character takes one, as an empty glyph box.
fn cells(c: char, col: usize) -> usize {
    if c == '\t' {
        TAB_WIDTH - col % TAB_WIDTH
    } else if c.is_ascii() && !c.is_ascii_control() {
        1
    } else {
        c.width().unwrap_or(1)
    }
}

/// A long line's rows, wrapped at `cols` cells. Row `i` shows bytes
/// `starts[i]..starts[i + 1]` of the line (the last row runs to the end).
#[derive(Debug, PartialEq)]
pub(crate) struct Rows {
    starts: Vec<usize>,
    /// Whether each row ends with the whitespace it wrapped at.
    spaces: Vec<bool>,
    len: usize,
}

impl Rows {
    /// Greedy word wrap at `cols` cells: a row breaks after the last
    /// whitespace that fits, or mid-word when a word is wider than the row.
    /// Whitespace never starts a row; it hangs past the edge, as it does in
    /// the shaped panes. Reads the line in `chunks` (a rope's), so it is
    /// never copied whole.
    pub(crate) fn wrap<'a>(chunks: impl IntoIterator<Item = &'a str>, cols: usize) -> Self {
        let cols = cols.max(1);
        let mut starts = vec![0];
        let mut spaces = Vec::new();
        let mut col = 0;
        // Byte just after the last whitespace on this row, and the column
        // there. Only non-whitespace follows it, whose cells don't depend
        // on the column, so the part of a word past it moves down as is.
        let mut after_space: Option<(usize, usize)> = None;
        let mut base = 0;
        for chunk in chunks {
            for (i, c) in chunk.char_indices() {
                let i = base + i;
                if c.is_whitespace() {
                    col += cells(c, col);
                    after_space = Some((i + c.len_utf8(), col));
                    continue;
                }
                let w = cells(c, col);
                let row_start = *starts.last().expect("one row at least");
                if col + w > cols && i > row_start && w > 0 {
                    match after_space.filter(|&(b, _)| b > row_start) {
                        Some((b, at_col)) => {
                            spaces.push(true);
                            starts.push(b);
                            col -= at_col;
                        }
                        None => {
                            spaces.push(false);
                            starts.push(i);
                            col = 0;
                        }
                    }
                    after_space = None;
                }
                col += w;
            }
            base += chunk.len();
        }
        spaces.push(false);
        Self {
            starts,
            spaces,
            len: base,
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.starts.len()
    }

    /// Bytes of row `row`.
    pub(crate) fn range(&self, row: usize) -> Range<usize> {
        let start = self.starts[row];
        let end = self.starts.get(row + 1).copied().unwrap_or(self.len);
        start..end
    }

    /// The row holding `byte`; at a wrap point, the row it starts.
    pub(crate) fn row_of(&self, byte: usize) -> usize {
        self.starts
            .partition_point(|&s| s <= byte)
            .saturating_sub(1)
    }

    /// Caret geometry for the whole line, `row_height` points a row and
    /// `cell` points a column. Only rows in `window` get their clusters;
    /// the rest have their bounds and nothing to hit or measure, so ask
    /// only about rows in the window. `row_text(range)` returns the line's
    /// bytes in `range`.
    pub(crate) fn geometry(
        &self,
        window: Range<usize>,
        cell: f32,
        row_height: f32,
        mut row_text: impl FnMut(Range<usize>) -> String,
    ) -> LineGeometry {
        let window = window.start.min(self.count())..window.end.min(self.count());
        let rows = (0..self.count())
            .map(|i| {
                let range = self.range(i);
                let clusters = if window.contains(&i) {
                    clusters(&row_text(range.clone()), range.start, cell, self.spaces[i])
                } else {
                    Vec::new()
                };
                Row {
                    top: i as f32 * row_height,
                    height: row_height,
                    start: range.start,
                    end: range.end,
                    clusters,
                    ends_in_space: self.spaces[i],
                }
            })
            .collect();
        LineGeometry { rows }
    }
}

/// One cluster per character of `row` (which starts at byte `base` of its
/// line), at its cells; combining marks join the character before. When the
/// row wrapped at whitespace, that trailing run is one cluster of its first
/// character's width, as the shaper keeps it: End and clicks past the row
/// stop after the word, not out where the hanging spaces would reach.
fn clusters(row: &str, base: usize, cell: f32, ends_in_space: bool) -> Vec<ClusterSpan> {
    let mut out: Vec<ClusterSpan> = Vec::with_capacity(row.len());
    let mut col = 0;
    // First cluster of the whitespace run the row ends with, if any so far.
    let mut run: Option<usize> = None;
    for (i, c) in row.char_indices() {
        let w = cells(c, col);
        let (start, end) = (base + i, base + i + c.len_utf8());
        if w == 0
            && let Some(last) = out.last_mut()
        {
            last.end = end;
            continue;
        }
        if !c.is_whitespace() {
            run = None;
        } else if run.is_none() {
            run = Some(out.len());
        }
        out.push(ClusterSpan {
            start,
            end,
            x: col as f32 * cell,
            w: w.max(1) as f32 * cell,
        });
        col += w;
    }
    if ends_in_space && let Some(first) = run {
        let end = out.last().map_or(0, |c| c.end);
        out.truncate(first + 1);
        out[first].end = end;
    }
    out
}

/// What the live pane shows instead of a block (or, before the first
/// parse, a raw line) holding a line over [`LONG_LINE`].
pub(crate) fn notice(bytes: usize) -> String {
    let bytes = bytes as f64;
    let size = if bytes >= 1024.0 * 1024.0 {
        format!("{:.1} MB", bytes / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB", bytes / 1024.0)
    };
    format!("A {size} block with a very long line. Edit it in the code pane.")
}

/// Whether a block spanning `range` holds a line over [`LONG_LINE`].
pub(crate) fn holds_long_line(doc: &inkmark_buffer::Document, range: &Range<usize>) -> bool {
    if range.len() <= LONG_LINE {
        return false;
    }
    let first = doc.byte_to_line(range.start);
    let last = doc.byte_to_line(range.end.saturating_sub(1).max(range.start));
    (first..=last).any(|l| doc.line_range(l).len() > LONG_LINE)
}

/// The source a notice for the block at `range` stands for: the block, but
/// its trailing newline. The notice's end is then the end of its text, and
/// the caret after the newline is on the next line, as after any paragraph.
pub(crate) fn notice_source(doc: &inkmark_buffer::Document, range: Range<usize>) -> Range<usize> {
    // A byte, not a slice: until the full parse lands, a rebased block can
    // end inside a character.
    let end = if range.end > range.start
        && range.end <= doc.len()
        && doc.rope().byte(range.end - 1) == b'\n'
    {
        range.end - 1
    } else {
        range.end
    };
    range.start..end
}

/// Whether the live pane shows `block` as a notice rather than shaping it.
/// A table cell lies on one line, so only that line is checked: between a
/// whole-table edit and the full parse a rebased cell can span the whole
/// table, and walking its lines for every cell is what that would cost.
pub(crate) fn shows_notice(doc: &inkmark_buffer::Document, block: &inkmark_parse::Block) -> bool {
    let range = &block.range;
    if block.kind.is_table_part() {
        range.len() > LONG_LINE && doc.line_range(doc.byte_to_line(range.start)).len() > LONG_LINE
    } else {
        holds_long_line(doc, range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(text: &str, cols: usize) -> Vec<&str> {
        let rows = Rows::wrap([text], cols);
        (0..rows.count()).map(|i| &text[rows.range(i)]).collect()
    }

    #[test]
    fn wraps_after_the_last_space_that_fits() {
        assert_eq!(rows_of("aaa bbb ccc", 8), ["aaa bbb ", "ccc"]);
        assert_eq!(rows_of("aaa bbb ccc", 7), ["aaa bbb ", "ccc"]);
        assert_eq!(rows_of("aaa bbb ccc", 6), ["aaa ", "bbb ", "ccc"]);
        // Spaces hang past the edge rather than starting a row.
        assert_eq!(rows_of("aaa      bbb", 4), ["aaa      ", "bbb"]);
    }

    #[test]
    fn a_word_wider_than_the_row_breaks_mid_word() {
        assert_eq!(rows_of("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(rows_of("ab cdefghij", 4), ["ab ", "cdef", "ghij"]);
    }

    #[test]
    fn wide_characters_take_two_cells_and_tabs_go_to_stops() {
        assert_eq!(rows_of("漢字漢字", 4), ["漢字", "漢字"]);
        assert_eq!(rows_of("a\tbc", 6), ["a\tbc"]);
        assert_eq!(rows_of("a\tbcd", 6), ["a\t", "bcd"]);
    }

    #[test]
    fn rows_cover_every_byte_once() {
        let text = "Some prose, a [link](u), and `code`: ".repeat(50) + "end";
        let rows = Rows::wrap([text.as_str()], 37);
        let mut at = 0;
        for i in 0..rows.count() {
            let r = rows.range(i);
            assert_eq!(r.start, at);
            assert!(r.end > r.start || rows.count() == 1);
            at = r.end;
            assert_eq!(rows.row_of(r.start), i);
        }
        assert_eq!(at, text.len());
    }

    #[test]
    fn geometry_puts_each_character_on_its_cells() {
        let text = "ab\u{301}c 漢x";
        let rows = Rows::wrap([text], 80);
        let g = rows.geometry(0..1, 10.0, 20.0, |r| text[r].to_owned());
        assert_eq!(g.rows.len(), 1);
        let c = &g.rows[0].clusters;
        // a, b + combining acute, c, space, 漢 (two cells), x.
        assert_eq!(c.len(), 6);
        assert_eq!((c[1].start, c[1].end, c[1].x), (1, 4, 10.0));
        assert_eq!((c[4].x, c[4].w), (40.0, 20.0));
        assert_eq!(c[5].x, 60.0);
        assert_eq!(g.caret_x(0, text.len()), 70.0);
    }

    #[test]
    fn only_the_window_has_clusters() {
        let text = "word ".repeat(100);
        let rows = Rows::wrap([text.as_str()], 20);
        let g = rows.geometry(2..3, 10.0, 20.0, |r| text[r].to_owned());
        assert_eq!(g.rows.len(), rows.count());
        assert!(g.rows[0].clusters.is_empty() && !g.rows[2].clusters.is_empty());
        assert_eq!(g.rows[3].top, 60.0);
        assert_eq!(g.height(), rows.count() as f32 * 20.0);
    }

    #[test]
    fn wrapping_reads_across_chunks() {
        let text = "aaa bbb\tccc ddd 漢字 eeeeeeeee f";
        let whole = Rows::wrap([text], 6);
        for split in (0..=text.len()).filter(|&i| text.is_char_boundary(i)) {
            assert_eq!(Rows::wrap([&text[..split], &text[split..]], 6), whole);
        }
    }

    #[test]
    fn hanging_spaces_are_one_cluster_and_end_stops_after_the_word() {
        let text = "a a      bbb";
        let rows = Rows::wrap([text], 4);
        let g = rows.geometry(0..2, 10.0, 20.0, |r| text[r].to_owned());
        let c = &g.rows[0].clusters;
        // a, the space between the words, a, then the six hanging spaces.
        assert_eq!(c.len(), 4);
        assert_eq!((c[1].start, c[1].end), (1, 2));
        assert_eq!((c[3].start, c[3].end, c[3].x, c[3].w), (3, 9, 30.0, 10.0));
        assert_eq!(g.hit_row_affine(0, f32::INFINITY), (3, false));
        assert_eq!(g.hit_row_affine(0, 200.0), (3, false));
    }

    #[test]
    fn a_notice_source_ending_inside_a_character_is_kept_whole() {
        let doc = inkmark_buffer::Document::from_text("ab中\ncd\n");
        assert_eq!(notice_source(&doc, 0..4), 0..4);
        assert_eq!(notice_source(&doc, 0..6), 0..5);
        assert_eq!(notice_source(&doc, 6..9), 6..8);
    }
}
