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

/// Where `#anchor` points: the start of the heading's text whose GitHub
/// slug is `anchor`. Repeated headings get `-1`, `-2`… as on GitHub.
pub fn heading_offset(doc: &Document, out: &ParseOutput, anchor: &str) -> Option<usize> {
    let anchor = anchor.to_lowercase();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for block in out.blocks.iter() {
        let BlockKind::Heading(_) = block.kind else {
            continue;
        };
        let spans = out.map.spans_in(block.range.clone());
        let mut text = String::new();
        let mut first_text = None;
        for s in &spans {
            match &s.kind {
                SpanKind::Text => text.push_str(&doc.slice(s.range.clone())),
                SpanKind::Replaced(r) => text.push_str(r),
                _ => continue,
            }
            first_text.get_or_insert(s.range.start);
        }
        let base = slug(&text);
        let count = seen.entry(base.clone()).or_insert(0);
        let id = if *count == 0 {
            base
        } else {
            format!("{base}-{count}")
        };
        *count += 1;
        if id == anchor {
            return Some(first_text.unwrap_or(block.range.start));
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
}
