use std::ops::Range;

use egui::{Rect, Vec2, pos2, vec2};

/// One shaped cluster: bytes `start..end` of the line drawn at `x..x + w`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClusterSpan {
    pub start: usize,
    pub end: usize,
    pub x: f32,
    pub w: f32,
}

/// One visual (wrapped) row of a line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub top: f32,
    pub height: f32,
    /// Bytes of the line this row shows.
    pub start: usize,
    pub end: usize,
    /// Clusters in visual order.
    pub clusters: Vec<ClusterSpan>,
    /// The last cluster is the whitespace the row wrapped at. (A row can
    /// also wrap mid-word: a long word, CJK text, after a hyphen.)
    pub ends_in_space: bool,
}

/// Caret and hit-test geometry of one laid-out line, in points relative to
/// the line's top-left corner. Byte offsets are relative to the line start.
///
/// Assumes left-to-right text; bidi caret movement comes later.
#[derive(Clone, Debug, PartialEq)]
pub struct LineGeometry {
    pub rows: Vec<Row>,
}

impl LineGeometry {
    pub fn height(&self) -> f32 {
        self.rows.last().map_or(0.0, |r| r.top + r.height)
    }

    /// Row showing the caret at `byte`. At a wrap point the caret belongs to
    /// the start of the next row.
    pub fn row_of(&self, byte: usize) -> usize {
        self.rows.iter().rposition(|r| r.start <= byte).unwrap_or(0)
    }

    /// Like [`row_of`](Self::row_of), but with `upstream` a caret at a wrap
    /// point belongs to the end of the row before (where End put it).
    pub fn row_of_affine(&self, byte: usize, upstream: bool) -> usize {
        let row = self.row_of(byte);
        if upstream && row > 0 && self.rows[row].start == byte && self.rows[row - 1].end == byte {
            row - 1
        } else {
            row
        }
    }

    pub fn caret_x(&self, row: usize, byte: usize) -> f32 {
        let row = &self.rows[row];
        for c in &row.clusters {
            if byte <= c.start {
                return c.x;
            }
            if byte < c.end {
                // Inside a ligature or multi-byte cluster: interpolate.
                return c.x + c.w * (byte - c.start) as f32 / (c.end - c.start) as f32;
            }
        }
        row.clusters.last().map_or(0.0, |c| c.x + c.w)
    }

    /// A thin caret rectangle at `byte`.
    pub fn caret_rect(&self, byte: usize, width: f32) -> Rect {
        self.caret_rect_affine(byte, width, false)
    }

    /// A caret rectangle at `byte`, at the end of the row before when
    /// `upstream` and `byte` is a wrap point.
    pub fn caret_rect_affine(&self, byte: usize, width: f32, upstream: bool) -> Rect {
        let row = self.row_of_affine(byte, upstream);
        let x = self.caret_x(row, byte);
        let r = &self.rows[row];
        Rect::from_min_size(pos2(x, r.top), vec2(width, r.height))
    }

    /// The byte nearest to `x` on `row`, for clicks and vertical movement.
    pub fn hit_row(&self, row: usize, x: f32) -> usize {
        self.hit_row_affine(row, x).0
    }

    /// [`hit_row`](Self::hit_row), and whether the byte is the end of this
    /// row that is also where the next row starts: the caret there belongs
    /// to this row (upstream), not the next.
    pub fn hit_row_affine(&self, row: usize, x: f32) -> (usize, bool) {
        let r = &self.rows[row];
        for c in &r.clusters {
            if x < c.x + c.w / 2.0 {
                return (c.start, false);
            }
        }
        // Past the end of a wrapped row. Stop before the space it wrapped
        // at, if that's one of its clusters; otherwise after its last
        // character. When that is also where the next row starts (a
        // mid-word wrap), the caret is upstream so it stays on this row.
        let next_start = self.rows.get(row + 1).map(|n| n.start);
        match (r.clusters.last(), next_start) {
            (Some(last), Some(_)) if r.ends_in_space => (last.start, false),
            (Some(last), Some(next)) => (last.end, last.end == next),
            _ => (r.end, false),
        }
    }

    pub fn hit(&self, pos: Vec2) -> usize {
        self.hit_affine(pos).0
    }

    /// [`hit`](Self::hit), with the affinity of
    /// [`hit_row_affine`](Self::hit_row_affine).
    pub fn hit_affine(&self, pos: Vec2) -> (usize, bool) {
        let row = self
            .rows
            .iter()
            .position(|r| pos.y < r.top + r.height)
            .unwrap_or(self.rows.len().saturating_sub(1));
        self.hit_row_affine(row, pos.x)
    }

    /// Selection highlight for line bytes `range`. `through_newline` adds a
    /// sliver after the last row when the selection continues to the next line.
    pub fn selection_rects(
        &self,
        range: Range<usize>,
        through_newline: bool,
        newline_width: f32,
        out: &mut Vec<Rect>,
    ) {
        for (i, r) in self.rows.iter().enumerate() {
            let is_last = i + 1 == self.rows.len();
            let start = range.start.max(r.start);
            let end = range.end.min(r.end);
            let mut x0 = self.caret_x(i, start);
            let mut x1 = self.caret_x(i, end);
            if start > end || (start == end && !(is_last && through_newline)) {
                continue;
            }
            if is_last && through_newline {
                x1 += newline_width;
            }
            if x1 < x0 {
                std::mem::swap(&mut x0, &mut x1);
            }
            out.push(Rect::from_min_max(
                pos2(x0, r.top),
                pos2(x1, r.top + r.height),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monospace rows of 10pt clusters: `rows` lists each row's byte range.
    fn mono(rows: &[Range<usize>]) -> LineGeometry {
        LineGeometry {
            rows: rows
                .iter()
                .enumerate()
                .map(|(i, r)| Row {
                    ends_in_space: false,
                    top: i as f32 * 20.0,
                    height: 20.0,
                    start: r.start,
                    end: r.end,
                    clusters: r
                        .clone()
                        .map(|b| ClusterSpan {
                            start: b,
                            end: b + 1,
                            x: (b - r.start) as f32 * 10.0,
                            w: 10.0,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    /// [`mono`] for `text`: rows ending in a space cluster say so.
    fn mono_text(text: &str, rows: &[Range<usize>]) -> LineGeometry {
        let mut g = mono(rows);
        for row in &mut g.rows {
            row.ends_in_space = row
                .clusters
                .last()
                .is_some_and(|c| text[c.start..c.end].chars().all(char::is_whitespace));
        }
        g
    }

    #[test]
    fn caret_positions_and_wrap_affinity() {
        // "hello world" wrapped as "hello " | "world".
        let g = mono_text("hello world", &[0..6, 6..11]);
        assert_eq!(g.row_of(0), 0);
        assert_eq!(g.row_of(5), 0);
        assert_eq!(g.row_of(6), 1);
        assert_eq!(g.caret_x(0, 3), 30.0);
        assert_eq!(g.caret_x(1, 11), 50.0);
        assert_eq!(g.caret_rect(6, 2.0).min, pos2(0.0, 20.0));
        assert_eq!(g.height(), 40.0);
    }

    #[test]
    fn hits_round_to_nearest_cluster_edge() {
        let g = mono_text("hello world", &[0..6, 6..11]);
        assert_eq!(g.hit(vec2(14.0, 5.0)), 1);
        assert_eq!(g.hit(vec2(16.0, 5.0)), 2);
        // Past the end of a wrapped row stays on that row.
        assert_eq!(g.hit(vec2(500.0, 5.0)), 5);
        // Past the end of the last row goes to the line end.
        assert_eq!(g.hit(vec2(500.0, 25.0)), 11);
        // Below everything: last row.
        assert_eq!(g.hit(vec2(0.0, 999.0)), 6);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)] // one row, not a range of rows
    fn empty_line_has_one_row_at_zero() {
        let g = mono(&[0..0]);
        assert_eq!(g.row_of(0), 0);
        assert_eq!(g.caret_x(0, 0), 0.0);
        assert_eq!(g.hit(vec2(50.0, 5.0)), 0);
        let mut rects = Vec::new();
        g.selection_rects(0..0, true, 5.0, &mut rects);
        assert_eq!(
            rects,
            vec![Rect::from_min_max(pos2(0.0, 0.0), pos2(5.0, 20.0))]
        );
    }

    #[test]
    fn selection_spans_rows() {
        let g = mono(&[0..6, 6..11]);
        let mut rects = Vec::new();
        g.selection_rects(3..8, false, 5.0, &mut rects);
        assert_eq!(
            rects,
            vec![
                Rect::from_min_max(pos2(30.0, 0.0), pos2(60.0, 20.0)),
                Rect::from_min_max(pos2(0.0, 20.0), pos2(20.0, 40.0)),
            ]
        );
    }

    #[test]
    fn ligature_cluster_interpolates() {
        let g = LineGeometry {
            rows: vec![Row {
                ends_in_space: false,
                top: 0.0,
                height: 20.0,
                start: 0,
                end: 2,
                clusters: vec![ClusterSpan {
                    start: 0,
                    end: 2,
                    x: 0.0,
                    w: 12.0,
                }],
            }],
        };
        assert_eq!(g.caret_x(0, 1), 6.0);
    }

    #[test]
    fn past_a_wrapped_rows_end_stays_on_that_row() {
        // Wrapped at a space that's a cluster of the row: before it.
        let g = mono_text("abc def", &[0..4, 4..7]);
        assert_eq!(g.hit_row_affine(0, f32::INFINITY), (3, false));
        // Wrapped mid-word ("abcde" | "fgh"): after the last character,
        // which is also the next row's start, so upstream.
        let g = mono_text("abcdefgh", &[0..5, 5..8]);
        assert_eq!(g.hit_row_affine(0, f32::INFINITY), (5, true));
        // A click on the right half of the last character is past it too.
        assert_eq!(g.hit_affine(vec2(46.0, 5.0)), (5, true));
        // The space in neither row's clusters: after the last character,
        // not at a wrap point.
        let mut g = mono_text("abcd ef", &[0..4, 5..7]);
        g.rows[0].end = 4;
        assert_eq!(g.hit_row_affine(0, f32::INFINITY), (4, false));
        // The last row ends at its end.
        let g = mono_text("abcdefgh", &[0..5, 5..8]);
        assert_eq!(g.hit_row_affine(1, f32::INFINITY), (8, false));
    }

    #[test]
    fn an_upstream_caret_shows_at_the_end_of_the_row_before() {
        let g = mono_text("abcdefgh", &[0..5, 5..8]);
        assert_eq!(g.row_of(5), 1);
        assert_eq!(g.row_of_affine(5, true), 0);
        assert_eq!(g.caret_rect_affine(5, 2.0, true).min, pos2(50.0, 0.0));
        assert_eq!(g.caret_rect_affine(5, 2.0, false).min, pos2(0.0, 20.0));
        // Not a wrap point: affinity changes nothing.
        assert_eq!(g.row_of_affine(3, true), 0);
        assert_eq!(g.row_of_affine(6, true), 1);
    }
}
