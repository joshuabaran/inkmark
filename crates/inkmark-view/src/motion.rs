//! Caret movement over a [`Document`]: graphemes, words and lines. Offsets are
//! bytes; every result lands on a grapheme boundary.

use std::ops::Range;

use inkmark_buffer::Document;
use unicode_segmentation::UnicodeSegmentation;

fn line_start(doc: &Document, offset: usize) -> usize {
    doc.line_to_byte(doc.byte_to_line(offset))
}

fn line_end(doc: &Document, offset: usize) -> usize {
    doc.line_range(doc.byte_to_line(offset)).end
}

/// Bytes either side of the caret one step reads. A grapheme or a word is
/// far shorter (a longer word is crossed in steps this long), and a step in
/// a very long line reads this much, not the rest of the line.
const REACH: usize = 4096;

/// The char boundary at or before `offset`.
fn floor_boundary(doc: &Document, offset: usize) -> usize {
    let rope = doc.rope();
    rope.char_to_byte(rope.byte_to_char(offset))
}

/// Where a step back from `offset` starts reading: its line's start, or
/// `REACH` bytes back.
fn reach_back(doc: &Document, offset: usize) -> usize {
    let start = line_start(doc, offset);
    if offset - start <= REACH {
        start
    } else {
        floor_boundary(doc, offset - REACH)
    }
}

/// Where a step forward from `offset` stops reading: its line's end, or
/// `REACH` bytes on.
fn reach_ahead(doc: &Document, offset: usize) -> usize {
    let end = line_end(doc, offset);
    if end - offset <= REACH {
        end
    } else {
        floor_boundary(doc, offset + REACH)
    }
}

pub fn prev_grapheme(doc: &Document, offset: usize) -> usize {
    let start = line_start(doc, offset);
    if offset == start {
        return offset.saturating_sub(1);
    }
    let from = reach_back(doc, offset);
    let text = doc.slice(from..offset);
    from + text
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(i, _)| i)
}

pub fn next_grapheme(doc: &Document, offset: usize) -> usize {
    let end = line_end(doc, offset);
    if offset == end {
        return (offset + 1).min(doc.len());
    }
    let text = doc.slice(offset..reach_ahead(doc, offset));
    offset + text.graphemes(true).next().map_or(0, str::len)
}

/// Start of the word before `offset`, skipping whitespace. Punctuation runs
/// count as words. At a line start, steps over the newline.
pub fn prev_word(doc: &Document, offset: usize) -> usize {
    let start = line_start(doc, offset);
    if offset == start {
        return prev_grapheme(doc, offset);
    }
    let from = reach_back(doc, offset);
    let text = doc.slice(from..offset);
    from + text
        .split_word_bound_indices()
        .rev()
        .find(|(_, w)| !w.trim().is_empty())
        .map_or(0, |(i, _)| i)
}

/// End of the word after `offset`, skipping whitespace. At a line end, steps
/// over the newline.
pub fn next_word(doc: &Document, offset: usize) -> usize {
    let end = line_end(doc, offset);
    if offset == end {
        return next_grapheme(doc, offset);
    }
    let text = doc.slice(offset..reach_ahead(doc, offset));
    offset
        + text
            .split_word_bound_indices()
            .find(|(_, w)| !w.trim().is_empty())
            .map_or(text.len(), |(i, w)| i + w.len())
}

/// The word (or whitespace / punctuation run) under `offset`, for double-click.
pub fn word_at(doc: &Document, offset: usize) -> Range<usize> {
    let line = doc.line_range(doc.byte_to_line(offset));
    let text = doc.slice(line.clone());
    let local = offset - line.start;
    let mut prev = None;
    for (i, w) in text.split_word_bound_indices() {
        if local < i + w.len() {
            return line.start + i..line.start + i + w.len();
        }
        prev = Some(line.start + i..line.start + i + w.len());
    }
    // At the line end: the last word, if any.
    prev.unwrap_or(offset..offset)
}

/// The whole line containing `offset`, including its newline, for triple-click.
pub fn line_at(doc: &Document, offset: usize) -> Range<usize> {
    let range = doc.line_range(doc.byte_to_line(offset));
    range.start..(range.end + 1).min(doc.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphemes_step_over_clusters_and_newlines() {
        // "e\u{301}" is one grapheme of 3 bytes; 🇯🇵 is one of 8.
        let doc = Document::from_text("ae\u{301}🇯🇵\nb");
        assert_eq!(next_grapheme(&doc, 0), 1);
        assert_eq!(next_grapheme(&doc, 1), 4);
        assert_eq!(next_grapheme(&doc, 4), 12);
        assert_eq!(next_grapheme(&doc, 12), 13);
        assert_eq!(prev_grapheme(&doc, 13), 12);
        assert_eq!(prev_grapheme(&doc, 12), 4);
        assert_eq!(prev_grapheme(&doc, 4), 1);
        assert_eq!(prev_grapheme(&doc, 0), 0);
        assert_eq!(next_grapheme(&doc, doc.len()), doc.len());
    }

    #[test]
    fn words_skip_whitespace_and_cross_lines() {
        let doc = Document::from_text("hello, big  world\nnext");
        assert_eq!(next_word(&doc, 0), 5);
        assert_eq!(next_word(&doc, 5), 6);
        assert_eq!(next_word(&doc, 6), 10);
        assert_eq!(next_word(&doc, 10), 17);
        assert_eq!(next_word(&doc, 17), 18);
        assert_eq!(prev_word(&doc, 17), 12);
        assert_eq!(prev_word(&doc, 12), 7);
        assert_eq!(prev_word(&doc, 7), 5);
        assert_eq!(prev_word(&doc, 18), 17);
    }

    #[test]
    fn word_and_line_selection() {
        let doc = Document::from_text("one two\nthree");
        assert_eq!(word_at(&doc, 5), 4..7);
        assert_eq!(word_at(&doc, 3), 3..4);
        assert_eq!(word_at(&doc, 7), 4..7);
        assert_eq!(line_at(&doc, 2), 0..8);
        assert_eq!(line_at(&doc, 10), 8..13);
        let empty = Document::from_text("");
        assert_eq!(word_at(&empty, 0), 0..0);
    }
}
