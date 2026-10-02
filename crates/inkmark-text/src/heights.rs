use std::ops::Range;

/// Per-line heights (in points) with prefix sums, for virtualized scrolling.
///
/// Heights start as estimates and are replaced as lines are laid out. Prefix
/// sums live in a Fenwick tree so offset and hit lookups stay O(log n) on
/// 100k+ lines.
pub struct HeightCache {
    heights: Vec<f32>,
    measured: Vec<bool>,
    measured_count: usize,
    /// 1-based Fenwick tree over `heights`, summed in f64 to avoid drift.
    tree: Vec<f64>,
}

/// Scroll position as "this line is at the top, scrolled `offset` points into it".
///
/// Anchoring to a line instead of an absolute y keeps the view still when
/// lines above it are measured and their heights change.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollAnchor {
    pub line: usize,
    pub offset: f32,
}

impl HeightCache {
    pub fn new(estimates: impl IntoIterator<Item = f32>) -> Self {
        let mut cache = Self {
            heights: Vec::new(),
            measured: Vec::new(),
            measured_count: 0,
            tree: Vec::new(),
        };
        cache.reset_estimates(estimates);
        cache
    }

    /// Replaces every height with a fresh estimate and forgets all measurements.
    pub fn reset_estimates(&mut self, estimates: impl IntoIterator<Item = f32>) {
        self.heights.clear();
        self.heights.extend(estimates);
        self.measured.clear();
        self.measured.resize(self.heights.len(), false);
        self.measured_count = 0;
        self.rebuild_tree();
    }

    fn rebuild_tree(&mut self) {
        let n = self.heights.len();
        self.tree.clear();
        self.tree.resize(n + 1, 0.0);
        for i in 1..=n {
            self.tree[i] += f64::from(self.heights[i - 1]);
            let parent = i + i.isolate_lowest_one();
            if parent <= n {
                self.tree[parent] += self.tree[i];
            }
        }
    }

    pub fn len(&self) -> usize {
        self.heights.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    pub fn height(&self, line: usize) -> f32 {
        self.heights[line]
    }

    pub fn is_measured(&self, line: usize) -> bool {
        self.measured[line]
    }

    pub fn measured_count(&self) -> usize {
        self.measured_count
    }

    /// Records the laid-out height of `line`.
    pub fn set_measured(&mut self, line: usize, height: f32) {
        self.set(line, height, true);
    }

    fn set(&mut self, line: usize, height: f32, measured: bool) {
        if self.measured[line] != measured {
            self.measured[line] = measured;
            if measured {
                self.measured_count += 1;
            } else {
                self.measured_count -= 1;
            }
        }
        let delta = f64::from(height) - f64::from(self.heights[line]);
        self.heights[line] = height;
        if delta != 0.0 {
            let mut i = line + 1;
            while i < self.tree.len() {
                self.tree[i] += delta;
                i += i.isolate_lowest_one();
            }
        }
    }

    /// Replaces lines `range` with unmeasured lines of the given estimated
    /// heights. O(log n) per line when the line count is unchanged (typing
    /// within a line), O(n) otherwise.
    pub fn splice(&mut self, range: Range<usize>, estimates: impl ExactSizeIterator<Item = f32>) {
        if estimates.len() == range.len() {
            for (line, h) in range.zip(estimates) {
                self.set(line, h, false);
            }
            return;
        }
        let removed_measured = self.measured[range.clone()].iter().filter(|&&m| m).count();
        let added = estimates.len();
        self.heights.splice(range.clone(), estimates);
        self.measured
            .splice(range, std::iter::repeat_n(false, added));
        self.measured_count -= removed_measured;
        self.rebuild_tree();
    }

    pub fn total(&self) -> f64 {
        self.offset_of(self.len())
    }

    /// Sum of the heights of lines `0..line`.
    pub fn offset_of(&self, line: usize) -> f64 {
        let mut sum = 0.0;
        let mut i = line.min(self.len());
        while i > 0 {
            sum += self.tree[i];
            i &= i - 1;
        }
        sum
    }

    /// The line containing document y-coordinate `y`, clamped to the document.
    pub fn line_at(&self, y: f64) -> ScrollAnchor {
        if self.is_empty() {
            return ScrollAnchor::default();
        }
        // Fenwick descent: the largest `line` with offset_of(line) <= y.
        let mut line = 0;
        let mut remaining = y.max(0.0);
        let mut step = self.len().next_power_of_two();
        while step > 0 {
            let next = line + step;
            if next <= self.len() && self.tree[next] <= remaining {
                line = next;
                remaining -= self.tree[next];
            }
            step >>= 1;
        }
        if line >= self.len() {
            let last = self.len() - 1;
            return ScrollAnchor {
                line: last,
                offset: self.heights[last],
            };
        }
        ScrollAnchor {
            line,
            offset: (remaining as f32).min(self.heights[line]),
        }
    }

    pub fn anchor_y(&self, anchor: ScrollAnchor) -> f64 {
        self.offset_of(anchor.line) + f64::from(anchor.offset)
    }

    /// Moves `anchor` by `delta` points, keeping a `viewport` tall window inside the document.
    pub fn scroll_by(&self, anchor: ScrollAnchor, delta: f32, viewport: f32) -> ScrollAnchor {
        let max = (self.total() - f64::from(viewport)).max(0.0);
        let y = (self.anchor_y(anchor) + f64::from(delta)).clamp(0.0, max);
        self.line_at(y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_offset(heights: &[f32], line: usize) -> f64 {
        heights[..line].iter().map(|&h| f64::from(h)).sum()
    }

    #[test]
    fn prefix_sums_match_naive_after_updates() {
        let mut heights: Vec<f32> = (0..1000).map(|i| 10.0 + (i % 7) as f32).collect();
        let mut cache = HeightCache::new(heights.iter().copied());
        for i in (0..1000).step_by(13) {
            let h = 20.0 + (i % 5) as f32 * 18.0;
            cache.set_measured(i, h);
            heights[i] = h;
        }
        for line in [0, 1, 2, 63, 64, 65, 500, 999, 1000] {
            assert!((cache.offset_of(line) - naive_offset(&heights, line)).abs() < 1e-6);
        }
        assert_eq!(cache.measured_count(), 77);
    }

    #[test]
    fn line_at_finds_containing_line() {
        let cache = HeightCache::new([10.0, 20.0, 30.0]);
        assert_eq!(
            cache.line_at(0.0),
            ScrollAnchor {
                line: 0,
                offset: 0.0
            }
        );
        assert_eq!(
            cache.line_at(9.5),
            ScrollAnchor {
                line: 0,
                offset: 9.5
            }
        );
        assert_eq!(
            cache.line_at(10.0),
            ScrollAnchor {
                line: 1,
                offset: 0.0
            }
        );
        assert_eq!(
            cache.line_at(45.0),
            ScrollAnchor {
                line: 2,
                offset: 15.0
            }
        );
        assert_eq!(
            cache.line_at(-5.0),
            ScrollAnchor {
                line: 0,
                offset: 0.0
            }
        );
        assert_eq!(
            cache.line_at(1e9),
            ScrollAnchor {
                line: 2,
                offset: 30.0
            }
        );
    }

    #[test]
    fn line_at_round_trips_offset_of() {
        let cache = HeightCache::new((0..10_000).map(|i| 14.0 + (i % 3) as f32 * 14.0));
        for line in (0..10_000).step_by(997) {
            assert_eq!(cache.line_at(cache.offset_of(line)).line, line);
        }
    }

    #[test]
    fn scroll_clamps_to_document() {
        let cache = HeightCache::new([10.0; 10]);
        let top = cache.scroll_by(ScrollAnchor::default(), -50.0, 30.0);
        assert_eq!(top, ScrollAnchor::default());
        let bottom = cache.scroll_by(top, 1000.0, 30.0);
        assert_eq!(cache.anchor_y(bottom), 70.0);
        let short = HeightCache::new([10.0; 2]);
        assert_eq!(
            short.scroll_by(ScrollAnchor::default(), 5.0, 30.0),
            ScrollAnchor::default()
        );
    }

    #[test]
    fn splice_matches_naive() {
        let mut heights: Vec<f32> = (0..200).map(|i| 10.0 + (i % 4) as f32).collect();
        let mut cache = HeightCache::new(heights.iter().copied());
        cache.set_measured(50, 99.0);
        heights[50] = 99.0;
        // Same count: in place, and the line becomes unmeasured again.
        cache.splice(50..51, [12.0].into_iter());
        heights[50] = 12.0;
        assert_eq!(cache.measured_count(), 0);
        // Grow and shrink.
        cache.splice(10..12, [1.0, 2.0, 3.0, 4.0].into_iter());
        heights.splice(10..12, [1.0, 2.0, 3.0, 4.0]);
        cache.splice(100..150, [7.0].into_iter());
        heights.splice(100..150, [7.0]);
        assert_eq!(cache.len(), heights.len());
        for line in [0, 10, 13, 14, 99, 100, 101, heights.len()] {
            assert!((cache.offset_of(line) - naive_offset(&heights, line)).abs() < 1e-6);
        }
    }

    #[test]
    fn anchor_holds_when_lines_above_are_measured() {
        let mut cache = HeightCache::new([10.0; 100]);
        let anchor = cache.line_at(500.0);
        assert_eq!(anchor.line, 50);
        cache.set_measured(10, 40.0);
        // Same anchor, new absolute position: the view does not jump.
        assert_eq!(cache.anchor_y(anchor), 530.0);
        assert_eq!(cache.scroll_by(anchor, 10.0, 100.0).line, 51);
    }
}
