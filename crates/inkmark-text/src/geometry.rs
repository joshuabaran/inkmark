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
        let row = self.row_of(byte);
        let x = self.caret_x(row, byte);
        let r = &self.rows[row];
        Rect::from_min_size(pos2(x, r.top), vec2(width, r.height))
    }

    /// The byte nearest to `x` on `row`, for clicks and vertical movement.
    pub fn hit_row(&self, row: usize, x: f32) -> usize {
        let r = &self.rows[row];
        for c in &r.clusters {
            if x < c.x + c.w / 2.0 {
                return c.start;
            }
        }
        let is_last = row + 1 == self.rows.len();
        match r.clusters.last() {
            // Past the end of a wrapped row: stay before its trailing
            // cluster rather than jumping to the next row.
            Some(last) if !is_last => last.start,
            _ => r.end,
        }
    }

    pub fn hit(&self, pos: Vec2) -> usize {
        let row = self
            .rows
            .iter()
            .position(|r| pos.y < r.top + r.height)
            .unwrap_or(self.rows.len().saturating_sub(1));
        self.hit_row(row, pos.x)
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

    #[test]
    fn caret_positions_and_wrap_affinity() {
        // "hello world" wrapped as "hello " | "world".
        let g = mono(&[0..6, 6..11]);
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
        let g = mono(&[0..6, 6..11]);
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
}
