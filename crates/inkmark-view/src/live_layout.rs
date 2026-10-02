//! Display text for one leaf block in the live view: hidden syntax dropped
//! (unless revealed), soft breaks as spaces, entities rendered, with a map
//! back to source bytes for the caret and hit-testing.

use std::ops::Range;

use egui::Color32;
use inkmark_buffer::Document;
use inkmark_parse::{BlockKind, Leaf, SourceMap, Span, SpanKind, Style, Syntax};
use inkmark_text::FontStyle;

use crate::theme;

/// A run of display text and the source bytes it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub display: Range<usize>,
    pub source: Range<usize>,
    /// Byte-for-byte copy of the source; otherwise (entities, soft breaks)
    /// the run maps to its source as a whole.
    pub exact: bool,
}

/// One visual line of a leaf: hard breaks and code-block newlines split them.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Segment {
    pub text: String,
    pub runs: Vec<(Range<usize>, FontStyle)>,
    pub colors: Vec<(Range<usize>, Color32)>,
    pub pieces: Vec<Piece>,
    /// Display ranges with a line through them (GFM strikethrough).
    pub strikes: Vec<Range<usize>>,
    /// Source offset of the segment start, for placing a caret in an empty one.
    pub source_start: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeafStyle {
    Paragraph,
    Heading(u8),
    Code,
    Html,
    Rule,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LeafLayout {
    pub style: LeafStyle,
    pub segments: Vec<Segment>,
}

/// Font size of heading levels 1–6, in percent of body text.
const HEADING_SCALE: [u16; 6] = [190, 155, 130, 115, 100, 90];

pub(crate) fn heading_scale(level: u8) -> u16 {
    HEADING_SCALE[usize::from(level.clamp(1, 6)) - 1]
}

struct Builder<'a> {
    src: &'a Document,
    style: LeafStyle,
    segments: Vec<Segment>,
    /// The span being added is struck-through text (not its `~~`).
    strike: bool,
}

impl Builder<'_> {
    fn segment(&mut self) -> &mut Segment {
        self.segments.last_mut().expect("always one segment")
    }

    fn font(&self, span: &Span) -> FontStyle {
        let s = span.style;
        let code = matches!(self.style, LeafStyle::Code | LeafStyle::Html)
            || s.contains(Style::CODE)
            || s.contains(Style::CODE_BLOCK)
            || s.contains(Style::HTML);
        FontStyle {
            mono: code,
            bold: s.contains(Style::STRONG) || span.heading > 0,
            italic: s.contains(Style::EMPHASIS) || s.contains(Style::IMAGE),
            scale: if span.heading > 0 {
                heading_scale(span.heading)
            } else {
                100
            },
        }
    }

    /// Shows source bytes as they are, starting a new segment at each newline.
    fn exact(&mut self, range: Range<usize>, font: FontStyle, color: Option<Color32>) {
        let text = self.src.slice(range.clone()).into_owned();
        let mut at = range.start;
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                // The newline itself isn't shown; the next line starts after it.
                at += 1;
                self.segments.push(Segment {
                    source_start: at,
                    ..Segment::default()
                });
            }
            if !part.is_empty() {
                self.push(part, at..at + part.len(), true, font, color);
            }
            at += part.len();
        }
    }

    fn push(
        &mut self,
        text: &str,
        source: Range<usize>,
        exact: bool,
        font: FontStyle,
        color: Option<Color32>,
    ) {
        let strike = self.strike;
        let seg = self.segment();
        let start = seg.text.len();
        seg.text.push_str(text);
        let display = start..seg.text.len();
        if font != FontStyle::default() {
            seg.runs.push((display.clone(), font));
        }
        if let Some(c) = color {
            seg.colors.push((display.clone(), c));
        }
        if strike {
            seg.strikes.push(display.clone());
        }
        seg.pieces.push(Piece {
            display,
            source,
            exact,
        });
    }
}

/// What to show as raw Markdown around the caret.
#[derive(Clone, Debug)]
pub(crate) struct Reveal {
    /// The caret's source line: block markers (`#`, fences, rules, hard
    /// breaks) on it are shown.
    pub line: Range<usize>,
    /// Inline elements (emphasis, code, links, images) the caret is in or
    /// touching show their delimiters; escapes and entities next to it
    /// show their source.
    pub caret: usize,
}

/// Inline style flags whose delimiters reveal per element.
const INLINE: [Style; 6] = [
    Style::EMPHASIS,
    Style::STRONG,
    Style::STRIKE,
    Style::CODE,
    Style::LINK,
    Style::IMAGE,
];

/// Byte ranges of the inline elements touching `caret`: for each inline
/// flag on a span at the caret, the contiguous run of spans with that flag.
fn elements_at(spans: &[inkmark_parse::Span], caret: usize) -> Vec<Range<usize>> {
    let touches = |s: &inkmark_parse::Span| s.range.start <= caret && caret <= s.range.end;
    let mut runs = Vec::new();
    for flag in INLINE {
        let Some(i) = spans
            .iter()
            .position(|s| touches(s) && s.style.contains(flag))
        else {
            continue;
        };
        let (mut a, mut b) = (i, i);
        while a > 0 && spans[a - 1].style.contains(flag) {
            a -= 1;
        }
        while b + 1 < spans.len() && spans[b + 1].style.contains(flag) {
            b += 1;
        }
        runs.push(spans[a].range.start..spans[b].range.end);
    }
    runs
}

/// Lays out `leaf`, rendered except for the syntax `reveal` picks out
/// (shown dimmed).
pub(crate) fn build(
    doc: &Document,
    map: &SourceMap,
    leaf: &Leaf,
    reveal: Option<Reveal>,
) -> LeafLayout {
    let range = leaf.block.range.clone();
    let style = match leaf.block.kind {
        BlockKind::Heading(level) => LeafStyle::Heading(level),
        BlockKind::CodeBlock { .. } => LeafStyle::Code,
        BlockKind::HtmlBlock => LeafStyle::Html,
        BlockKind::ThematicBreak => LeafStyle::Rule,
        _ => LeafStyle::Paragraph,
    };
    let mut b = Builder {
        src: doc,
        style,
        segments: vec![Segment {
            source_start: range.start,
            ..Segment::default()
        }],
        strike: false,
    };
    let spans = map.spans_in(range.clone());
    let elements = reveal
        .as_ref()
        .map_or_else(Vec::new, |v| elements_at(&spans, v.caret));
    let on_line = |r: &Range<usize>| {
        reveal.as_ref().is_some_and(|v| {
            r.start <= v.line.end && (r.end > v.line.start || r.start == v.line.start)
        })
    };
    let touching = |r: &Range<usize>| {
        reveal
            .as_ref()
            .is_some_and(|v| r.start <= v.caret && v.caret <= r.end)
    };
    let in_element = |r: &Range<usize>| {
        elements
            .iter()
            .any(|e| e.start <= r.start && r.end <= e.end)
    };
    for span in spans.iter().cloned() {
        let r = span.range.start.max(range.start)..span.range.end.min(range.end);
        if r.is_empty() {
            continue;
        }
        let font = b.font(&span);
        b.strike = span.style.contains(Style::STRIKE)
            && matches!(span.kind, SpanKind::Text | SpanKind::Replaced(_));
        let raw = match span.kind {
            SpanKind::Syntax(Syntax::Delimiter | Syntax::LinkMarkup) => in_element(&r),
            SpanKind::Syntax(Syntax::Escape) | SpanKind::Replaced(_) => touching(&r),
            _ => on_line(&r),
        };
        match &span.kind {
            SpanKind::Text => b.exact(r, font, theme::live_color(&span)),
            SpanKind::Replaced(text) if !raw && r == span.range => {
                b.push(text, r, false, font, theme::live_color(&span))
            }
            SpanKind::Replaced(_) => b.exact(r, font, theme::live_color(&span)),
            SpanKind::SoftBreak => b.push(" ", r, false, font, None),
            SpanKind::Syntax(Syntax::HardBreak) => {
                if raw {
                    b.exact(r, font, Some(theme::MARKUP));
                } else {
                    let end = r.end;
                    b.segments.push(Segment {
                        source_start: end,
                        ..Segment::default()
                    });
                }
            }
            // Container prefixes, indentation and table markup are drawn as
            // bars, bullets, margins and grid lines, so they stay hidden even
            // on the caret line.
            SpanKind::Syntax(k)
                if raw
                    && !matches!(
                        k,
                        Syntax::QuotePrefix
                            | Syntax::ListMarker
                            | Syntax::TableMarkup
                            | Syntax::FootnoteLabel
                    ) =>
            {
                let font = FontStyle {
                    bold: false,
                    italic: false,
                    ..font
                };
                b.exact(r, font, Some(theme::syntax_color(&span)));
            }
            SpanKind::Syntax(_) | SpanKind::Whitespace => {}
        }
    }
    // A trailing newline at the end of the block doesn't start a new line.
    while b.segments.len() > 1 && b.segments.last().is_some_and(|s| s.text.is_empty()) {
        b.segments.pop();
    }
    LeafLayout {
        style,
        segments: b.segments,
    }
}

impl LeafLayout {
    /// Display position (segment, byte) of source offset `src`. Offsets in
    /// hidden syntax snap forward to the next shown text.
    pub fn display_pos(&self, src: usize) -> (usize, usize) {
        for (si, seg) in self.segments.iter().enumerate() {
            if seg.pieces.is_empty() && src <= seg.source_start {
                return (si, 0);
            }
            for p in &seg.pieces {
                if src < p.source.start {
                    return (si, p.display.start);
                }
                if src <= p.source.end {
                    let d = if p.exact {
                        p.display.start + (src - p.source.start)
                    } else if src == p.source.start {
                        p.display.start
                    } else {
                        p.display.end
                    };
                    return (si, d);
                }
            }
        }
        let last = self.segments.len() - 1;
        (last, self.segments[last].text.len())
    }

    /// Source offset shown at display position `d` of segment `seg`. At a
    /// boundary between two pieces the later one wins, so a caret before
    /// "bold" in `**bold**` lands inside the emphasis.
    pub fn source_pos(&self, seg: usize, d: usize) -> usize {
        let s = &self.segments[seg];
        for p in &s.pieces {
            if d < p.display.end {
                let d = d.max(p.display.start);
                return if p.exact {
                    p.source.start + (d - p.display.start)
                } else if d == p.display.start {
                    p.source.start
                } else {
                    p.source.end
                };
            }
        }
        s.pieces.last().map_or(s.source_start, |p| p.source.end)
    }
}

#[cfg(test)]
mod tests {
    use inkmark_parse::{MarkdownParser, PulldownParser};

    use super::*;

    fn layouts(src: &str, reveal_line: Option<usize>) -> (Document, Vec<LeafLayout>) {
        let doc = Document::from_text(src);
        let out = PulldownParser.parse(src);
        let reveal = reveal_line.map(|l| Reveal {
            line: doc.line_range(l),
            caret: doc.line_range(l).start,
        });
        let leaves = out
            .blocks
            .leaves_from(0)
            .map(|leaf| build(&doc, &out.map, &leaf, reveal.clone()))
            .collect();
        (doc, leaves)
    }

    fn texts(l: &LeafLayout) -> Vec<&str> {
        l.segments.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn hides_syntax_and_joins_soft_breaks() {
        let (_, l) = layouts(
            "# Title *em*\n\nSome **bold** &amp; `code`\nnext \\* line\n",
            None,
        );
        assert_eq!(l[0].style, LeafStyle::Heading(1));
        assert_eq!(texts(&l[0]), vec!["Title em"]);
        assert_eq!(texts(&l[1]), vec!["Some bold & code next * line"]);
    }

    fn layout_at(src: &str, caret: usize) -> LeafLayout {
        let doc = Document::from_text(src);
        let out = PulldownParser.parse(src);
        let line = doc.line_range(doc.byte_to_line(caret));
        let leaf = out.blocks.leaves_from(caret).next().unwrap();
        build(&doc, &out.map, &leaf, Some(Reveal { line, caret }))
    }

    #[test]
    fn reveals_only_the_element_at_the_caret() {
        let src = "Some **bold** and *em* with `code` &amp; \\* [a](u)\n";
        // Caret inside "bold": only its delimiters show.
        let l = layout_at(src, src.find("old").unwrap());
        assert_eq!(texts(&l), vec!["Some **bold** and em with code & * a"]);
        // Touching the start of the code span reveals its backticks.
        let l = layout_at(src, src.find('`').unwrap());
        assert_eq!(texts(&l), vec!["Some bold and em with `code` & * a"]);
        // Next to the entity and the escape: their source.
        let l = layout_at(src, src.find("&amp;").unwrap() + 2);
        assert_eq!(texts(&l), vec!["Some bold and em with code &amp; * a"]);
        let l = layout_at(src, src.find("\\*").unwrap());
        assert_eq!(texts(&l), vec!["Some bold and em with code & \\* a"]);
        // Inside the link text: brackets and destination.
        let l = layout_at(src, src.find("[a").unwrap() + 1);
        assert_eq!(texts(&l), vec!["Some bold and em with code & * [a](u)"]);
    }

    #[test]
    fn block_markers_reveal_on_the_caret_line() {
        let src = "## Title *x*\n";
        let l = layout_at(src, src.find("Ti").unwrap());
        assert_eq!(texts(&l), vec!["## Title x"]);
    }

    #[test]
    fn strikethrough_text_is_marked_but_not_its_delimiters() {
        let doc = Document::from_text("a ~~old~~ b\n");
        let out = inkmark_parse::GfmParser.parse("a ~~old~~ b\n");
        let leaf = out.blocks.leaves_from(0).next().unwrap();
        let l = build(&doc, &out.map, &leaf, None);
        assert_eq!(texts(&l), vec!["a old b"]);
        assert_eq!(l.segments[0].strikes, vec![2..5]);
        let l = build(
            &doc,
            &out.map,
            &leaf,
            Some(Reveal {
                line: 0..12,
                caret: 5,
            }),
        );
        assert_eq!(texts(&l), vec!["a ~~old~~ b"]);
        assert_eq!(l.segments[0].strikes, vec![4..7]);
    }

    #[test]
    fn hard_breaks_and_code_blocks_split_segments() {
        let (_, l) = layouts("a  \nb\n\n```rust\nfn x\nfn y\n```\n", None);
        assert_eq!(texts(&l[0]), vec!["a", "b"]);
        assert_eq!(l[1].style, LeafStyle::Code);
        assert_eq!(texts(&l[1]), vec!["fn x", "fn y"]);
        // On the fence line, the fence shows.
        let (_, l) = layouts("```rust\nfn x\n```\n", Some(0));
        assert_eq!(texts(&l[0]), vec!["```rust", "fn x"]);
    }

    #[test]
    fn list_and_quote_prefixes_are_hidden() {
        let (_, l) = layouts("> - one\n>   *two*\n", None);
        assert_eq!(texts(&l[0]), vec!["one two"]);
        // Even on the caret line (in the emphasis), prefixes stay hidden.
        let src = "> - one\n>   *two*\n";
        let l = layout_at(src, src.find("two").unwrap());
        assert_eq!(texts(&l), vec!["one *two*"]);
    }

    #[test]
    fn caret_maps_between_source_and_display() {
        let src = "Some **bold** &amp; x\n";
        let (_, l) = layouts(src, None);
        let l = &l[0];
        assert_eq!(texts(l), vec!["Some bold & x"]);
        let bold = src.find("bold").unwrap();
        assert_eq!(l.display_pos(bold), (0, 5));
        // Inside the hidden "**" snaps to the next shown text.
        assert_eq!(l.display_pos(bold - 1), (0, 5));
        assert_eq!(l.source_pos(0, 5), bold);
        assert_eq!(l.source_pos(0, 7), bold + 2);
        // The entity is atomic: its start or its end.
        let amp = src.find("&amp;").unwrap();
        assert_eq!(l.display_pos(amp + 2), (0, 11));
        assert_eq!(l.source_pos(0, 11), amp + 5);
        // Round trip for every visible position.
        for d in 0..=l.segments[0].text.len() {
            let s = l.source_pos(0, d);
            assert_eq!(l.display_pos(s).0, 0);
        }
    }

    #[test]
    fn a_footnote_reference_shows_its_label_until_the_caret_touches_it() {
        use inkmark_parse::GfmParser;

        let src = "Text[^1] more.\n\n[^1]: Note.\n";
        let doc = Document::from_text(src);
        let out = GfmParser.parse(src);
        let leaf = out.blocks.leaves_from(0).next().unwrap();
        let at = |caret: usize| {
            let line = doc.line_range(doc.byte_to_line(caret));
            build(&doc, &out.map, &leaf, Some(Reveal { line, caret }))
        };
        assert_eq!(texts(&at(0)), vec!["Text[1] more."]);
        assert_eq!(texts(&at(4)), vec!["Text[^1] more."]);
        assert_eq!(texts(&at(8)), vec!["Text[^1] more."]);
        assert_eq!(texts(&at(10)), vec!["Text[1] more."]);
        // The definition's label is a margin marker: never in the text.
        let note = out.blocks.leaves_from(16).next().unwrap();
        let l = build(
            &doc,
            &out.map,
            &note,
            Some(Reveal {
                line: doc.line_range(2),
                caret: 16,
            }),
        );
        assert_eq!(texts(&l), vec!["Note."]);
    }
}
