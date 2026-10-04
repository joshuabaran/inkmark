//! Display text for one leaf block in the live view: hidden syntax dropped
//! (unless revealed), soft breaks as spaces, entities rendered, with a map
//! back to source bytes for the caret and hit-testing.

use std::ops::Range;

use egui::Color32;
use inkmark_buffer::Document;
use inkmark_parse::{BlockKind, Leaf, SourceMap, Span, SpanKind, Style, Syntax};
use inkmark_text::FontStyle;

use crate::theme::Theme;

/// A run of display text and the source bytes it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub display: Range<usize>,
    pub source: Range<usize>,
    /// Byte-for-byte copy of the source; otherwise (entities, soft breaks)
    /// the run maps to its source as a whole.
    pub exact: bool,
}

/// A formula the live pane draws in place of its source. The bytes stay in
/// the document; this is only where the gap sits.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MathAtom {
    /// Display bytes of the stand-in. One object-replacement character until
    /// the view widens it to the formula's width.
    pub display: Range<usize>,
    /// Source bytes of the whole formula, delimiters included.
    pub source: Range<usize>,
    /// The formula between the delimiters.
    pub tex: String,
    pub display_style: bool,
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
    /// Formulas drawn over a stand-in gap in `text`.
    pub maths: Vec<MathAtom>,
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

    /// A typeset formula. Inline sits in the current segment. Display math
    /// takes a segment of its own so the row can grow to the formula's height.
    fn push_formula(
        &mut self,
        source: Range<usize>,
        tex: String,
        display_style: bool,
        font: FontStyle,
    ) {
        if display_style {
            let seg = self.segment();
            if !seg.text.is_empty() || !seg.pieces.is_empty() || !seg.maths.is_empty() {
                self.segments.push(Segment {
                    source_start: source.start,
                    ..Segment::default()
                });
            }
        }
        let strike = self.strike;
        self.strike = false;
        // U+FFFC is one cluster. The view replaces it once it knows the width.
        let end = source.end;
        self.push("\u{FFFC}", source.clone(), false, font, None);
        self.strike = strike;
        let seg = self.segment();
        let display = seg.pieces.last().expect("just pushed").display.clone();
        seg.maths.push(MathAtom {
            display,
            source,
            tex,
            display_style,
        });
        if display_style {
            self.segments.push(Segment {
                source_start: end,
                ..Segment::default()
            });
        }
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

/// `start..end` spans of one formula, when `spans[start]` opens one.
/// The closer is the matching `$` or `$$`. A later formula written against
/// this one stays its own formula, and an emphasis marker inside the body
/// is not a closer.
fn formula_end(doc: &Document, spans: &[Span], start: usize) -> Option<usize> {
    if !math_delim(doc, &spans[start]) {
        return None;
    }
    let width = spans[start].range.len();
    let mut i = start + 1;
    while i < spans.len()
        && spans[i].style.contains(Style::MATH)
        && spans[i].range.start == spans[i - 1].range.end
    {
        if math_delim(doc, &spans[i]) && spans[i].range.len() == width {
            return (spans[start].range.end <= spans[i].range.start).then_some(i + 1);
        }
        i += 1;
    }
    None
}

fn math_delim(doc: &Document, span: &Span) -> bool {
    if !span.style.contains(Style::MATH) {
        return false;
    }
    if !matches!(span.kind, SpanKind::Syntax(Syntax::Delimiter)) {
        return false;
    }
    let text = doc.slice(span.range.clone());
    text == "$" || text == "$$"
}

/// Lays out `leaf`, rendered except for the syntax `reveal` picks out
/// (shown dimmed). A formula whose source overlaps `show_source` is drawn
/// as its bytes: that is how a formula the renderer rejected stays text.
pub(crate) fn build(
    doc: &Document,
    map: &SourceMap,
    leaf: &Leaf,
    reveal: Option<Reveal>,
    theme: &Theme,
    show_source: &[Range<usize>],
    math_caret: Option<usize>,
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
    let mut i = 0;
    while i < spans.len() {
        if let Some(end) = formula_end(doc, &spans, i) {
            let full = spans[i].range.start..spans[end - 1].range.end;
            let inside = full.start.max(range.start)..full.end.min(range.end);
            let caret_in = reveal
                .as_ref()
                .is_some_and(|v| full.start <= v.caret && v.caret < full.end)
                || math_caret.is_some_and(|c| full.start <= c && c < full.end);
            let forced = show_source
                .iter()
                .any(|s| s.start < full.end && full.start < s.end);
            if inside == full && !caret_in && !forced {
                let open = spans[i].range.end;
                let close = spans[end - 1].range.start;
                let display_style = spans[i].range.len() == 2;
                let font_at = spans[i + 1..end - 1]
                    .iter()
                    .position(|s| matches!(s.kind, SpanKind::Text | SpanKind::Replaced(_)))
                    .map_or(i, |n| i + 1 + n);
                let font = b.font(&spans[font_at]);
                b.push_formula(
                    full,
                    doc.slice(open..close).into_owned(),
                    display_style,
                    font,
                );
            } else {
                for span in spans[i..end].iter().cloned() {
                    let r = span.range.start.max(range.start)..span.range.end.min(range.end);
                    if r.is_empty() {
                        continue;
                    }
                    let font = b.font(&span);
                    b.strike = span.style.contains(Style::STRIKE)
                        && matches!(span.kind, SpanKind::Text | SpanKind::Replaced(_));
                    let color = match span.kind {
                        SpanKind::Syntax(_) => Some(theme.markup),
                        _ => theme.live_color(&span),
                    };
                    b.exact(r, font, color);
                }
            }
            i = end;
            continue;
        }
        let span = spans[i].clone();
        i += 1;
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
            SpanKind::Text => b.exact(r, font, theme.live_color(&span)),
            SpanKind::Replaced(text) if !raw && r == span.range => {
                b.push(text, r, false, font, theme.live_color(&span))
            }
            SpanKind::Replaced(_) => b.exact(r, font, theme.live_color(&span)),
            SpanKind::SoftBreak => b.push(" ", r, false, font, None),
            SpanKind::Syntax(Syntax::HardBreak) => {
                if raw {
                    b.exact(r, font, Some(theme.markup));
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
                b.exact(r, font, Some(theme.syntax_color(&span)));
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
    /// "bold" in `**bold**` lands inside the emphasis. The interior of a
    /// typeset formula maps to its first byte.
    pub fn source_pos(&self, seg: usize, d: usize) -> usize {
        let s = &self.segments[seg];
        for math in &s.maths {
            if math.display.start <= d && d < math.display.end {
                return math.source.start;
            }
        }
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
            .map(|leaf| {
                build(
                    &doc,
                    &out.map,
                    &leaf,
                    reveal.clone(),
                    &Theme::dark(),
                    &[],
                    None,
                )
            })
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
        build(
            &doc,
            &out.map,
            &leaf,
            Some(Reveal { line, caret }),
            &Theme::dark(),
            &[],
            None,
        )
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
        let l = build(&doc, &out.map, &leaf, None, &Theme::dark(), &[], None);
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
            &Theme::dark(),
            &[],
            None,
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
            build(
                &doc,
                &out.map,
                &leaf,
                Some(Reveal { line, caret }),
                &Theme::dark(),
                &[],
                None,
            )
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
            &Theme::dark(),
            &[],
            None,
        );
        assert_eq!(texts(&l), vec!["Note."]);
    }

    fn gfm_layout(src: &str, caret: Option<usize>) -> LeafLayout {
        use inkmark_parse::GfmParser;

        let doc = Document::from_text(src);
        let out = GfmParser.parse(src);
        let leaf = out.blocks.leaves_from(0).next().unwrap();
        let reveal = caret.map(|caret| Reveal {
            line: doc.line_range(doc.byte_to_line(caret)),
            caret,
        });
        build(&doc, &out.map, &leaf, reveal, &Theme::dark(), &[], None)
    }

    #[test]
    fn a_formula_stays_out_of_the_text_until_the_caret_is_inside() {
        let src = "Energy $E=mc^2$ today.\n";
        let hidden = gfm_layout(src, None);
        assert_eq!(texts(&hidden), vec!["Energy \u{FFFC} today."]);
        assert_eq!(hidden.segments[0].maths.len(), 1);
        assert_eq!(hidden.segments[0].maths[0].tex, "E=mc^2");
        assert!(!hidden.segments[0].maths[0].display_style);
        let inside = src.find("mc").unwrap();
        let shown = gfm_layout(src, Some(inside));
        assert_eq!(texts(&shown), vec!["Energy $E=mc^2$ today."]);
        assert!(shown.segments.iter().all(|s| s.maths.is_empty()));
        // The byte just after the formula still typesets it.
        let after = src.find("today").unwrap();
        assert_eq!(
            texts(&gfm_layout(src, Some(after))),
            vec!["Energy \u{FFFC} today."]
        );
    }

    #[test]
    fn display_math_is_its_own_segment_and_code_is_not_math() {
        let src = "See\n\n$$\\frac{1}{2}$$\n";
        let l = gfm_layout(src, None);
        // The first leaf is "See", not the formula.
        assert_eq!(texts(&l), vec!["See"]);
        let src = "$$\n\\frac{1}{2}\n$$\n";
        let l = gfm_layout(src, None);
        // The segment pushed so later text starts a new row is dropped when
        // nothing follows, so a formula on its own is one row. The caret at
        // the formula's end stays on that row.
        assert_eq!(l.segments.len(), 1);
        let seg = &l.segments[0];
        assert!(
            seg.maths
                .iter()
                .any(|m| m.display_style && m.tex.contains("frac"))
        );
        assert!(!seg.text.contains("frac"));
        let end = seg.maths[0].source.end;
        assert_eq!(l.display_pos(end).0, 0);
        let inside = src.find("frac").unwrap();
        let shown = gfm_layout(src, Some(inside));
        let joined: String = texts(&shown).into_iter().collect::<Vec<_>>().join("\n");
        assert!(joined.contains("frac"));
        assert!(shown.segments.iter().all(|s| s.maths.is_empty()));

        let src = "use `$x$` here\n";
        let l = gfm_layout(src, None);
        assert_eq!(texts(&l), vec!["use $x$ here"]);
        assert!(l.segments.iter().all(|s| s.maths.is_empty()));

        // Text after a display formula keeps its own segment, and that
        // segment is the words, not an empty row.
        let src = "$$a$$ then\n";
        let l = gfm_layout(src, None);
        assert!(
            l.segments
                .iter()
                .any(|s| s.maths.iter().any(|m| m.tex == "a"))
        );
        let last = l.segments.last().unwrap();
        assert!(last.text.contains("then"), "{:?}", texts(&l));
        assert!(!last.text.is_empty());
    }

    #[test]
    fn adjacent_formulas_stay_apart() {
        let src = "$$a$$$$b$$\n";
        let l = gfm_layout(src, None);
        let tex: Vec<_> = l
            .segments
            .iter()
            .flat_map(|s| s.maths.iter().map(|m| m.tex.as_str()))
            .collect();
        assert_eq!(tex, vec!["a", "b"]);
    }

    #[test]
    fn a_rejected_formula_is_shown_as_source() {
        let src = "Energy $E=mc^2$ today.\n";
        let doc = Document::from_text(src);
        let out = inkmark_parse::GfmParser.parse(src);
        let leaf = out.blocks.leaves_from(0).next().unwrap();
        let open = src.find('$').unwrap();
        let forced = open..src.find("today").unwrap();
        let l = build(
            &doc,
            &out.map,
            &leaf,
            None,
            &Theme::dark(),
            std::slice::from_ref(&forced),
            None,
        );
        assert_eq!(texts(&l), vec!["Energy $E=mc^2$ today."]);
        assert!(l.segments.iter().all(|s| s.maths.is_empty()));
    }
}
