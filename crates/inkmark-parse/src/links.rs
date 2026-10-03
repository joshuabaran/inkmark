//! Links at a position, and the places in a document they lead to: what
//! Ctrl+click follows.

use std::collections::HashMap;

use inkmark_buffer::Document;
use pulldown_cmark::{BrokenLink, CowStr, Event, Parser, Tag};

use crate::map::{Span, SpanKind, Style};
use crate::pulldown::GFM_OPTIONS;
use crate::tree::BlockKind;
use crate::{ParseOutput, normalize_label};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// A link's destination as written (a URL, a path, `#anchor`), or the
    /// URL of a bare autolink (`www.` gets `http://`, an email `mailto:`).
    Dest(String),
    /// A footnote reference's label.
    Footnote(String),
}

/// Whether `offset` is on link or footnote-reference text. Cheap enough to
/// ask every frame for the pointer cursor.
pub fn is_link(out: &ParseOutput, offset: usize) -> bool {
    span_at(out, offset)
        .is_some_and(|s| s.style.contains(Style::LINK) || s.style.contains(Style::FOOTNOTE))
}

/// The link at `offset`, if any.
pub fn link_at(doc: &Document, out: &ParseOutput, offset: usize) -> Option<Link> {
    let span = span_at(out, offset)?;
    if span.style.contains(Style::FOOTNOTE) {
        let SpanKind::Replaced(shown) = &span.kind else {
            return None;
        };
        let label = shown.strip_prefix('[')?.strip_suffix(']')?;
        return Some(Link::Footnote(label.to_owned()));
    }
    if !span.style.contains(Style::LINK) {
        return None;
    }
    // Re-parse the block around it to get the destination, resolving
    // reference links (`[text][ref]`) through the document's definitions.
    let leaf = out
        .blocks
        .leaves_from(offset)
        .next()
        .map(|l| l.block.range)
        .filter(|r| r.start <= offset && offset < r.end)?;
    let text = doc.slice(leaf.clone()).into_owned();
    let rel = offset - leaf.start;
    let mut defs = |link: BrokenLink<'_>| {
        out.link_defs
            .get(&normalize_label(&link.reference))
            .map(|dest| (CowStr::from(dest.clone()), CowStr::from("")))
    };
    let parser = Parser::new_with_broken_link_callback(&text, GFM_OPTIONS, Some(&mut defs));
    for (event, range) in parser.into_offset_iter() {
        if let Event::Start(Tag::Link { dest_url, .. }) = event
            && range.start <= rel
            && rel < range.end
        {
            return Some(Link::Dest(dest_url.into_string()));
        }
    }
    // Not a Markdown link: a GFM autolink literal, which is link-styled
    // text. Its run of spans is the URL.
    let spans = out.map.spans_in(leaf);
    let i = spans.iter().position(|s| s.range.contains(&offset))?;
    let linked = |s: &Span| s.style.contains(Style::LINK) && s.kind == SpanKind::Text;
    let (mut a, mut b) = (i, i);
    while a > 0 && linked(&spans[a - 1]) && spans[a - 1].range.end == spans[a].range.start {
        a -= 1;
    }
    while b + 1 < spans.len()
        && linked(&spans[b + 1])
        && spans[b].range.end == spans[b + 1].range.start
    {
        b += 1;
    }
    let url = doc
        .slice(spans[a].range.start..spans[b].range.end)
        .into_owned();
    Some(Link::Dest(if url.starts_with("www.") {
        format!("http://{url}")
    } else if url.contains('@') && !url.contains(':') {
        format!("mailto:{url}")
    } else {
        url
    }))
}

/// One heading, in document order. `text` is the heading's words, trimmed:
/// the same characters a GitHub slug is built from. `offset` is the first of
/// those characters, or the block start when the heading has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub offset: usize,
}

/// Every heading in the block tree, including headings inside a list or a
/// quote. The outline and [`heading_offset`] both walk this list, so a click
/// and a Ctrl+click land on the same byte.
pub fn headings(doc: &Document, out: &ParseOutput) -> Vec<Heading> {
    let doc_len = doc.len();
    let mut found = Vec::new();
    for block in out.blocks.iter() {
        let BlockKind::Heading(level) = block.kind else {
            continue;
        };
        let spans = out.map.spans_in(block.range.clone());
        let mut text = String::new();
        let mut first_text = None;
        // Source offset of the first character that survives trimming.
        let mut content_at = None;
        for s in &spans {
            // A parse that has not caught up can name bytes past the rope.
            // Skipping them keeps a frame from panicking on the slice.
            if s.range.start > doc_len || s.range.end > doc_len {
                continue;
            }
            let piece = match &s.kind {
                SpanKind::Text => doc.slice(s.range.clone()),
                SpanKind::Replaced(rendered) => std::borrow::Cow::Borrowed(rendered.as_ref()),
                _ => continue,
            };
            first_text.get_or_insert(s.range.start);
            if content_at.is_none() {
                let rest = piece.trim_start();
                if !rest.is_empty() {
                    let at = match &s.kind {
                        // Text is the source bytes, so the trim is a byte shift.
                        SpanKind::Text => s.range.start + (piece.len() - rest.len()),
                        // A replacement (an entity) is not the source. The
                        // caret goes to the start of that span.
                        _ => s.range.start,
                    };
                    content_at = Some(at.min(doc_len));
                }
            }
            text.push_str(&piece);
        }
        found.push(Heading {
            level,
            text: text.trim().to_owned(),
            offset: content_at
                .or(first_text)
                .unwrap_or(block.range.start)
                .min(doc_len),
        });
    }
    found
}

/// Where `#anchor` points: the start of the heading's text whose GitHub
/// slug is `anchor`. Repeated headings get `-1`, `-2`… as on GitHub.
pub fn heading_offset(doc: &Document, out: &ParseOutput, anchor: &str) -> Option<usize> {
    let anchor = anchor.to_lowercase();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for heading in headings(doc, out) {
        let base = slug(&heading.text);
        let count = seen.entry(base.clone()).or_insert(0);
        let id = if *count == 0 {
            base
        } else {
            format!("{base}-{count}")
        };
        *count += 1;
        if id == anchor {
            return Some(heading.offset);
        }
    }
    None
}

/// GitHub's heading anchor: lowercase, punctuation dropped, spaces as `-`.
pub fn slug(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// Where footnote `label`'s definition starts: its first block's text.
pub fn footnote_offset(doc: &Document, out: &ParseOutput, label: &str) -> Option<usize> {
    let want = normalize_label(label);
    out.blocks
        .iter()
        .filter(|b| b.kind == BlockKind::FootnoteDefinition)
        .find(|b| {
            let line_end = doc.line_range(doc.byte_to_line(b.range.start)).end;
            let head = doc.slice(b.range.start..line_end.min(b.range.end));
            definition_label(&head).is_some_and(|l| normalize_label(l) == want)
        })
        .map(|b| {
            out.blocks
                .leaves_from(b.range.start)
                .next()
                .map(|l| l.block.range.start)
                .filter(|&s| s < b.range.end)
                .unwrap_or(b.range.start)
        })
}

/// The label of a footnote definition line (`[^label]: …`), as written.
/// Escaped brackets are part of it; an unescaped `[` means it isn't one.
pub fn definition_label(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("[^")?;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            ']' => return rest[i + 1..].starts_with(':').then(|| &rest[..i]),
            '[' => return None,
            _ => {}
        }
    }
    None
}

fn span_at(out: &ParseOutput, offset: usize) -> Option<Span> {
    out.map
        .spans_in(offset..offset + 1)
        .into_iter()
        .find(|s| s.range.start <= offset && offset < s.range.end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GfmParser, MarkdownParser};

    fn at(src: &str, needle: &str) -> Option<Link> {
        let doc = Document::from_text(src);
        let out = GfmParser.parse(src);
        link_at(&doc, &out, src.find(needle).unwrap())
    }

    fn dest(s: &str) -> Option<Link> {
        Some(Link::Dest(s.into()))
    }

    #[test]
    fn finds_the_destination_of_the_link_under_a_position() {
        let src =
            "See [the docs](docs/a%20b.md#setup) and [ref][r], <https://x.org>.\n\n[r]: other.md\n";
        assert_eq!(at(src, "the docs"), dest("docs/a%20b.md#setup"));
        assert_eq!(
            at(src, "docs/a"),
            dest("docs/a%20b.md#setup"),
            "on the revealed destination"
        );
        assert_eq!(at(src, "ref]"), dest("other.md"));
        assert_eq!(at(src, "x.org"), dest("https://x.org"));
        assert_eq!(at(src, "See"), None);
    }

    #[test]
    fn autolinks_and_footnotes() {
        let src = "Go to www.example.com/a_b or mail me@example.org.[^n]\n\n[^n]: Note.\n";
        assert_eq!(at(src, "example.com"), dest("http://www.example.com/a_b"));
        assert_eq!(at(src, "me@"), dest("mailto:me@example.org"));
        assert_eq!(at(src, "[^n]"), Some(Link::Footnote("n".into())));
    }

    #[test]
    fn anchors_follow_githubs_slugs() {
        let src = "# Getting Started!\n\ntext\n\n## Getting started\n\n## Ünïcode *and* `code`\n";
        let doc = Document::from_text(src);
        let out = GfmParser.parse(src);
        assert_eq!(heading_offset(&doc, &out, "getting-started"), Some(2));
        let second = src.find("Getting started").unwrap();
        assert_eq!(
            heading_offset(&doc, &out, "getting-started-1"),
            Some(second)
        );
        let third = src.find("Ünïcode").unwrap();
        assert_eq!(heading_offset(&doc, &out, "ünïcode-and-code"), Some(third));
        assert_eq!(heading_offset(&doc, &out, "Getting-Started"), Some(2));
        assert_eq!(heading_offset(&doc, &out, "missing"), None);
    }

    #[test]
    fn footnote_definitions_are_found_by_label() {
        let src = "A[^Note] b[^x\\]y].\n\n[^note]: First.\n\n[^x\\]y]:\n    Indented.\n";
        let doc = Document::from_text(src);
        let out = GfmParser.parse(src);
        assert_eq!(footnote_offset(&doc, &out, "Note"), src.find("First"));
        assert_eq!(footnote_offset(&doc, &out, "x\\]y"), src.find("Indented"));
        assert_eq!(footnote_offset(&doc, &out, "none"), None);
        assert_eq!(definition_label("[^a\\]b]: x"), Some("a\\]b"));
        assert_eq!(definition_label("[^a[b]: x"), None);
    }

    #[test]
    fn headings_match_the_anchor_offsets() {
        let src = "# Getting Started!\n\ntext\n\n## Getting started\n\n## Ünïcode *and* `code`\n";
        let doc = Document::from_text(src);
        let out = GfmParser.parse(src);
        let found = headings(&doc, &out);
        assert_eq!(
            found
                .iter()
                .map(|h| (h.level, h.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "Getting Started!"),
                (2, "Getting started"),
                (2, "Ünïcode and code"),
            ]
        );
        assert_eq!(
            found[0].offset,
            heading_offset(&doc, &out, "getting-started").unwrap()
        );
        assert_eq!(
            found[1].offset,
            heading_offset(&doc, &out, "getting-started-1").unwrap()
        );
        assert_eq!(found[2].offset, src.find("Ünïcode").unwrap());
    }

    #[test]
    fn headings_include_setext_quotes_and_lists() {
        let src = "Setext title\n============\n\nSub title\n---------\n\n> # Quoted\n\n- item\n\n  ## Listed\n\n```\n# Not a heading\n```\n\n###### Deep\n\n#   \n";
        let doc = Document::from_text(src);
        let out = GfmParser.parse(src);
        let found = headings(&doc, &out);
        assert_eq!(
            found
                .iter()
                .map(|h| (h.level, h.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (1, "Setext title"),
                (2, "Sub title"),
                (1, "Quoted"),
                (2, "Listed"),
                (6, "Deep"),
                (1, ""),
            ]
        );
        assert_eq!(found[0].offset, src.find("Setext title").unwrap());
        assert_eq!(found[2].offset, src.find("Quoted").unwrap());
        assert_eq!(found[3].offset, src.find("Listed").unwrap());
        assert!(found[5].offset < src.len());
        assert!(found.iter().all(|h| h.text != "Not a heading"));
    }

    #[test]
    fn a_heading_offset_skips_leading_space_in_its_text() {
        let src = "#  Hello\n";
        let doc = Document::from_text(src);
        let out = ParseOutput {
            blocks: crate::tree::BlockTree::from_blocks(vec![crate::tree::Block {
                kind: BlockKind::Heading(1),
                range: 0..src.len(),
                depth: 0,
            }]),
            map: crate::map::SourceMap::from_spans(vec![
                span(0..2, SpanKind::Whitespace),
                span(2..9, SpanKind::Text),
                span(9..10, SpanKind::Whitespace),
            ]),
            ..ParseOutput::default()
        };
        let found = headings(&doc, &out);
        assert_eq!(found[0].text, "Hello");
        assert_eq!(found[0].offset, src.find('H').unwrap());
        assert_eq!(
            heading_offset(&doc, &out, "hello"),
            Some(src.find('H').unwrap())
        );
    }

    #[test]
    fn a_span_past_the_document_is_not_sliced() {
        let doc = Document::from_text("Hi");
        let out = ParseOutput {
            blocks: crate::tree::BlockTree::from_blocks(vec![crate::tree::Block {
                kind: BlockKind::Heading(1),
                range: 0..20,
                depth: 0,
            }]),
            map: crate::map::SourceMap::from_spans(vec![span(0..20, SpanKind::Text)]),
            ..ParseOutput::default()
        };
        let found = headings(&doc, &out);
        assert_eq!(found[0].text, "");
        assert_eq!(found[0].offset, 0);
    }

    fn span(range: std::ops::Range<usize>, kind: SpanKind) -> crate::map::Span {
        crate::map::Span {
            range,
            kind,
            style: crate::map::Style::default(),
            heading: 1,
        }
    }
}
