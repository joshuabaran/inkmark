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

    /// Hides `body`, or shows it again when a fold of this heading is
    /// already stored. `child` is a nested heading's body start, when one
    /// sits inside `body`; that nested fold is left alone. An empty range
    /// is left alone.
    pub(crate) fn toggle(&mut self, body: Range<usize>, child: Option<usize>) {
        if body.is_empty() {
            return;
        }
        let limit = child.unwrap_or(body.end);
        if self.bodies.iter().any(|fold| owns(fold, &body, limit)) {
            self.bodies.retain(|fold| !owns(fold, &body, limit));
        } else {
            self.bodies.push(body);
        }
    }

    /// A fold of this heading is stored. A fold that starts a line or two
    /// into `body` still counts: Enter above a folded body leaves the new
    /// line visible and the rest hidden. A nested fold, beginning at
    /// `child`, does not.
    pub(crate) fn covers(&self, body: &Range<usize>, child: Option<usize>) -> bool {
        let limit = child.unwrap_or(body.end);
        self.bodies.iter().any(|fold| owns(fold, body, limit))
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

    /// Pulls each range onto whole lines. A start that landed mid-line
    /// moves back to that line, so the line stays hidden. An end that
    /// landed mid-line moves back to that line's start, so the partial
    /// line stays visible. A newline inserted at a boundary is already on
    /// the next line and stays outside.
    pub(crate) fn align_lines(&mut self, doc: &Document) {
        for range in &mut self.bodies {
            *range = line_aligned(doc, range.start, range.end);
        }
        self.bodies.retain(|range| !range.is_empty());
    }
}

/// `fold` belongs to the heading whose body is `body`: it starts in `body`,
/// before a nested heading at `limit`, and overlaps `body`.
fn owns(fold: &Range<usize>, body: &Range<usize>, limit: usize) -> bool {
    !fold.is_empty()
        && fold.start >= body.start
        && fold.start < limit
        && fold.start < body.end
        && body.start < fold.end
}

/// The whole lines of `start..end`. See [`Folds::align_lines`].
pub(crate) fn line_aligned(doc: &Document, mut start: usize, mut end: usize) -> Range<usize> {
    let len = doc.len();
    start = start.min(len);
    end = end.min(len).max(start);
    if start < len {
        let at = doc.line_to_byte(doc.byte_to_line(start));
        if at != start {
            start = at;
        }
    }
    if end < len {
        let at = doc.line_to_byte(doc.byte_to_line(end));
        if at != end {
            end = at;
        }
    }
    start..end.max(start)
}

#[cfg(test)]
mod tests {
    use inkmark_buffer::{EditKind, Selection};

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
        folds.toggle(10..20, None);
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
        folds.toggle(10..20, None);
        folds.rebase(&[change(10, 20, 10)]);
        assert!(folds.ranges().is_empty());
    }

    #[test]
    fn toggling_the_same_start_removes_the_fold() {
        let mut folds = Folds::default();
        folds.toggle(4..9, None);
        folds.toggle(4..12, None);
        assert!(folds.ranges().is_empty());
        folds.toggle(0..0, None);
        assert!(folds.ranges().is_empty());
    }

    #[test]
    fn text_at_the_body_start_stays_on_its_line_and_a_newline_stays_outside() {
        let mut doc = Document::from_text("# H\nbody\n");
        let mut folds = Folds::default();
        folds.toggle(4..9, None);
        doc.apply(
            vec![inkmark_buffer::Edit::insert(4, "XY")],
            Selection::caret(4),
            Selection::caret(6),
            EditKind::Other,
        )
        .unwrap();
        let changes: Vec<_> = doc.log().changes_since(0).unwrap().copied().collect();
        folds.rebase(&changes);
        folds.align_lines(&doc);
        assert_eq!(folds.ranges().len(), 1);
        assert_eq!(folds.ranges()[0], 4..11);

        let mut doc = Document::from_text("# H\nbody\n");
        let mut folds = Folds::default();
        folds.toggle(4..9, None);
        doc.apply(
            vec![inkmark_buffer::Edit::insert(4, "\n")],
            Selection::caret(4),
            Selection::caret(5),
            EditKind::Other,
        )
        .unwrap();
        let changes: Vec<_> = doc.log().changes_since(0).unwrap().copied().collect();
        folds.rebase(&changes);
        folds.align_lines(&doc);
        assert_eq!(folds.ranges().len(), 1);
        assert_eq!(folds.ranges()[0], 5..10);
        assert!(folds.covers(&(4..10), None));
        folds.toggle(4..10, None);
        assert!(folds.ranges().is_empty());
    }

    #[test]
    fn opening_a_heading_leaves_a_nested_fold() {
        let mut folds = Folds::default();
        folds.toggle(4..30, None);
        folds.toggle(16..30, None);
        folds.toggle(4..30, Some(16));
        assert_eq!(folds.ranges().len(), 1);
        assert_eq!(folds.ranges()[0], 16..30);
    }
}
