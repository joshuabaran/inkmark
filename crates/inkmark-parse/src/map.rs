use std::ops::Range;

use inkmark_buffer::{Bias, Change};

use crate::chunked::{Chunked, Ranged};

/// Inline and block context a span sits in, as bit flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style(u16);

impl Style {
    pub const EMPHASIS: Self = Self(1);
    pub const STRONG: Self = Self(1 << 1);
    pub const CODE: Self = Self(1 << 2);
    pub const LINK: Self = Self(1 << 3);
    pub const IMAGE: Self = Self(1 << 4);
    pub const HTML: Self = Self(1 << 5);
    pub const CODE_BLOCK: Self = Self(1 << 6);
    pub const HEADING: Self = Self(1 << 7);
    pub const QUOTE: Self = Self(1 << 8);
    pub const LIST: Self = Self(1 << 9);
    /// GFM `~~strikethrough~~`.
    pub const STRIKE: Self = Self(1 << 10);
    /// Inside a GFM table's header row.
    pub const TABLE_HEAD: Self = Self(1 << 11);
    /// A footnote reference, `[^label]`.
    pub const FOOTNOTE: Self = Self(1 << 12);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

/// Markdown syntax: present in the source, hidden in the live view unless
/// the caret reveals it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Syntax {
    /// Emphasis / strong / code-span delimiters.
    Delimiter,
    /// The backslash of a backslash escape.
    Escape,
    /// `#` markers or a setext underline.
    HeadingMarker,
    ListMarker,
    QuotePrefix,
    /// Code fence lines, including the info string.
    Fence,
    /// Link and image brackets, destination and title.
    LinkMarkup,
    /// Trailing spaces or backslash of a hard line break, with its newline.
    HardBreak,
    ThematicBreak,
    /// GFM table pipes, the delimiter row (`|:--|--:|`) and cell padding.
    TableMarkup,
    /// A GFM task list marker, `[ ] ` (false) or `[x] ` (true), including
    /// the space after it.
    TaskMarker(bool),
    /// A footnote definition's `[^label]:` and the space after it (with any
    /// container prefix on the same line). Drawn in the margin like a list
    /// marker.
    FootnoteLabel,
    /// Anything else the parser consumed without output, e.g. link reference
    /// definitions.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpanKind {
    /// Shown as-is: the source bytes are the rendered text.
    Text,
    /// Shown as different text, e.g. an entity (`&amp;` → `&`).
    Replaced(Box<str>),
    /// A newline inside a paragraph, rendered as a space.
    SoftBreak,
    Syntax(Syntax),
    /// Indentation, newlines and blank lines between or around blocks.
    Whitespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub kind: SpanKind,
    pub style: Style,
    /// Heading level (1–6) when `style` has `HEADING`, else 0.
    pub heading: u8,
}

/// Every byte of the source, classified: spans are sorted, non-empty and
/// contiguous from 0 to the source length.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceMap {
    spans: Chunked<Span>,
}

impl Ranged for Span {
    fn range(&self) -> &Range<usize> {
        &self.range
    }

    fn range_mut(&mut self) -> &mut Range<usize> {
        &mut self.range
    }
}

impl SourceMap {
    pub(crate) fn from_spans(spans: Vec<Span>) -> Self {
        Self {
            spans: Chunked::from_vec(spans),
        }
    }

    /// A map that knows nothing yet: one plain-text span over `len` bytes.
    pub fn unparsed(len: usize) -> Self {
        Self::from_spans(
            (len > 0)
                .then(|| Span {
                    range: 0..len,
                    kind: SpanKind::Text,
                    style: Style::default(),
                    heading: 0,
                })
                .into_iter()
                .collect(),
        )
    }

    /// All spans, in order.
    pub fn iter(&self) -> impl Iterator<Item = Span> + '_ {
        self.spans.iter()
    }

    pub fn span_count(&self) -> usize {
        self.spans.len()
    }

    pub fn len(&self) -> usize {
        self.spans.last().map_or(0, |s| s.range.end)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Spans overlapping `range`.
    pub fn spans_in(&self, range: Range<usize>) -> Vec<Span> {
        self.spans
            .iter_from(range.start)
            .skip_while(|s| s.range.end <= range.start)
            .take_while(|s| s.range.start < range.end)
            .collect()
    }

    /// Checks the coverage invariant against a source of `len` bytes.
    pub fn validate(&self, len: usize) -> Result<(), String> {
        let mut at = 0;
        for (i, s) in self.iter().enumerate() {
            if s.range.start != at {
                return Err(format!(
                    "span {i} starts at {} but expected {at}",
                    s.range.start
                ));
            }
            if s.range.is_empty() {
                return Err(format!("span {i} is empty at {at}"));
            }
            at = s.range.end;
        }
        if at != len {
            return Err(format!("spans end at {at}, source is {len} bytes"));
        }
        Ok(())
    }

    /// Shifts spans through an edit. Text inserted at a boundary joins the
    /// span before it; spans inside deleted text disappear.
    pub(crate) fn rebase(&mut self, change: &Change) {
        let new_len = change.map(self.len(), Bias::Right);
        let map = |offset: usize| {
            if offset == 0 {
                0
            } else {
                change.map(offset, Bias::Right)
            }
        };
        let delta = change.new_end as isize - change.old_end as isize;
        self.spans.rebase(
            change.start,
            change.old_end,
            delta,
            |r| map(r.start)..map(r.end),
            |s| !s.range.is_empty(),
        );
        if self.spans.len() == 0 && new_len > 0 {
            *self = Self::unparsed(new_len);
        }
    }

    /// Replaces the spans covering `region` with `spans` (already in document
    /// offsets and covering exactly `region`).
    pub(crate) fn splice(&mut self, region: Range<usize>, spans: Vec<Span>) {
        self.spans.edit_region(region.clone(), |v| {
            split_at(v, region.start);
            split_at(v, region.end);
            let first = v.partition_point(|s| s.range.end <= region.start);
            let last = v.partition_point(|s| s.range.start < region.end);
            v.splice(first..last, spans);
        });
    }
}

/// Ensures a span boundary at `offset`.
fn split_at(spans: &mut Vec<Span>, offset: usize) {
    let i = spans.partition_point(|s| s.range.end <= offset);
    let Some(span) = spans.get(i) else { return };
    if span.range.start < offset {
        let mut tail = span.clone();
        tail.range.start = offset;
        spans[i].range.end = offset;
        spans.insert(i + 1, tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(range: Range<usize>, kind: SpanKind) -> Span {
        Span {
            range,
            kind,
            style: Style::default(),
            heading: 0,
        }
    }

    fn sample() -> SourceMap {
        // "**ab** cd"
        SourceMap::from_spans(vec![
            span(0..2, SpanKind::Syntax(Syntax::Delimiter)),
            span(2..4, SpanKind::Text),
            span(4..6, SpanKind::Syntax(Syntax::Delimiter)),
            span(6..9, SpanKind::Text),
        ])
    }

    #[test]
    fn validate_catches_gaps_and_short_maps() {
        assert!(sample().validate(9).is_ok());
        assert!(sample().validate(10).is_err());
        let gap =
            SourceMap::from_spans(vec![span(0..2, SpanKind::Text), span(3..4, SpanKind::Text)]);
        assert!(gap.validate(4).is_err());
    }

    #[test]
    fn spans_in_returns_overlaps() {
        let map = sample();
        let kinds: Vec<_> = map.spans_in(3..5).iter().map(|s| s.range.clone()).collect();
        assert_eq!(kinds, vec![2..4, 4..6]);
        assert!(map.spans_in(9..9).is_empty());
    }

    #[test]
    fn rebase_keeps_coverage() {
        let mut map = sample();
        // Insert "X" at 3 (inside "ab").
        map.rebase(&Change {
            start: 3,
            old_end: 3,
            new_end: 4,
            ..Change::default()
        });
        assert!(map.validate(10).is_ok());
        assert_eq!(map.iter().nth(1).unwrap().range, 2..5);
        // Delete "**" at 5..7 entirely.
        map.rebase(&Change {
            start: 5,
            old_end: 7,
            new_end: 5,
            ..Change::default()
        });
        assert!(map.validate(8).is_ok());
        assert_eq!(map.span_count(), 3);
        // Insert at 0 stays covered.
        map.rebase(&Change {
            start: 0,
            old_end: 0,
            new_end: 2,
            ..Change::default()
        });
        assert!(map.validate(10).is_ok());
    }

    #[test]
    fn splice_replaces_a_region() {
        let mut map = sample();
        map.splice(3..7, vec![span(3..7, SpanKind::Whitespace)]);
        assert!(map.validate(9).is_ok());
        let ranges: Vec<_> = map.iter().map(|s| s.range).collect();
        assert_eq!(ranges, vec![0..2, 2..3, 3..7, 7..9]);
    }
}
