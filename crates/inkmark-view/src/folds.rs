//! Folds in the code pane. Each one is the source range of a heading's
//! body: the heading line stays visible, and the range moves with edits
//! so a reparse does not drop it.

use std::ops::Range;

use inkmark_buffer::{Bias, Change, Document};

/// Hidden source ranges. A line whose first byte sits in one of them is
/// not drawn. Empty ranges are dropped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Folds {
    bodies: Vec<Range<usize>>,
}

impl Folds {
    pub(crate) fn ranges(&self) -> &[Range<usize>] {
        &self.bodies
    }

    pub(crate) fn clear(&mut self) {
        self.bodies.clear();
    }

    /// Hides `body`, or shows it again when a fold already starts there.
    /// An empty range is left alone.
    pub(crate) fn toggle(&mut self, body: Range<usize>) {
        if body.is_empty() {
            return;
        }
        if let Some(i) = self
            .bodies
            .iter()
            .position(|range| range.start == body.start)
        {
            self.bodies.remove(i);
        } else {
            self.bodies.push(body);
        }
    }

    pub(crate) fn is_folded_at(&self, body_start: usize) -> bool {
        self.bodies.iter().any(|range| range.start == body_start)
    }

    /// The line's first byte is inside a hidden range.
    pub(crate) fn hides_line(&self, doc: &Document, line: usize) -> bool {
        if line >= doc.line_count() {
            return false;
        }
        let start = doc.line_to_byte(line);
        self.bodies
            .iter()
            .any(|range| range.start <= start && start < range.end)
    }

    /// The first line after every fold that covers `line`, or `None` when
    /// `line` is visible. The returned line may be `doc.line_count()`.
    pub(crate) fn hidden_until(&self, doc: &Document, line: usize) -> Option<usize> {
        if line >= doc.line_count() {
            return None;
        }
        let start = doc.line_to_byte(line);
        let end_byte = self
            .bodies
            .iter()
            .filter(|range| range.start <= start && start < range.end)
            .map(|range| range.end)
            .max()?;
        let next = if end_byte >= doc.len() {
            doc.line_count()
        } else {
            let at = doc.byte_to_line(end_byte);
            if doc.line_to_byte(at) < end_byte {
                at + 1
            } else {
                at
            }
        };
        Some(next.max(line + 1))
    }

    /// Drops every fold that hides the line containing `offset`.
    pub(crate) fn reveal(&mut self, doc: &Document, offset: usize) -> bool {
        if doc.line_count() == 0 {
            return false;
        }
        let line = doc.byte_to_line(offset.min(doc.len()));
        let start = doc.line_to_byte(line);
        let before = self.bodies.len();
        self.bodies
            .retain(|range| !(range.start <= start && start < range.end));
        self.bodies.len() != before
    }

    /// Moves the ranges through edits, oldest first. A range an edit
    /// removes is dropped.
    pub(crate) fn rebase(&mut self, changes: &[Change]) {
        for change in changes {
            for range in &mut self.bodies {
                let start = change.map(range.start, Bias::Right);
                let end = change.map(range.end, Bias::Left).max(start);
                *range = start..end;
            }
        }
        self.bodies.retain(|range| !range.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(start: usize, old_end: usize, new_end: usize) -> Change {
        Change {
            start,
            old_end,
            new_end,
            ..Change::default()
        }
    }

    #[test]
    fn an_insert_before_a_fold_shifts_it_and_one_inside_grows_it() {
        let mut folds = Folds::default();
        folds.toggle(10..20);
        folds.rebase(&[change(0, 0, 3)]);
        let shifted = 13..23;
        assert_eq!(folds.ranges(), std::slice::from_ref(&shifted));
        folds.rebase(&[change(15, 15, 18)]);
        let grown = 13..26;
        assert_eq!(folds.ranges(), std::slice::from_ref(&grown));
    }

    #[test]
    fn deleting_the_body_drops_the_fold() {
        let mut folds = Folds::default();
        folds.toggle(10..20);
        folds.rebase(&[change(10, 20, 10)]);
        assert!(folds.ranges().is_empty());
    }

    #[test]
    fn toggling_the_same_start_removes_the_fold() {
        let mut folds = Folds::default();
        folds.toggle(4..9);
        folds.toggle(4..12);
        assert!(folds.ranges().is_empty());
        folds.toggle(0..0);
        assert!(folds.ranges().is_empty());
    }
}
