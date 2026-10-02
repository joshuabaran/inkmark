//! GFM autolink literals (spec §6.9): `www.` links, `http(s)://` links and
//! email addresses in plain text. pulldown-cmark doesn't support these, so
//! after a GFM parse we restyle the matching text as links. The bytes are
//! still shown as typed, so the source map stays exact.

use std::ops::Range;

use crate::map::{SourceMap, Span, SpanKind, Style, Syntax};

/// Text inside these is never autolinked.
const EXCLUDED: [Style; 5] = [
    Style::LINK,
    Style::IMAGE,
    Style::CODE,
    Style::CODE_BLOCK,
    Style::HTML,
];

pub(crate) fn mark(src: &str, map: &mut SourceMap) {
    let spans: Vec<Span> = map.iter().collect();
    // GFM recognizes autolinks on the raw text, before emphasis or entities
    // are resolved (cmark-gfm matches them as it scans), so `a*b*c` and
    // `&amp;` inside a URL stay in it. Scan runs of adjacent inline text,
    // delimiters, escapes and entities as one piece of source.
    let eligible = |s: &Span| {
        matches!(
            s.kind,
            SpanKind::Text
                | SpanKind::Replaced(_)
                | SpanKind::Syntax(Syntax::Delimiter | Syntax::Escape)
        ) && !EXCLUDED.iter().any(|x| s.style.contains(*x))
    };
    let mut links: Vec<Range<usize>> = Vec::new();
    let mut i = 0;
    while i < spans.len() {
        if !eligible(&spans[i]) {
            i += 1;
            continue;
        }
        let start = spans[i].range.start;
        let mut end = spans[i].range.end;
        let mut j = i + 1;
        while j < spans.len() && eligible(&spans[j]) && spans[j].range.start == end {
            end = spans[j].range.end;
            j += 1;
        }
        links.extend(find(src, start..end));
        i = j;
    }
    if links.is_empty() {
        return;
    }
    let mut out: Vec<Span> = Vec::with_capacity(spans.len() + links.len() * 2);
    let mut next = 0;
    for span in spans {
        while next < links.len() && links[next].end <= span.range.start {
            next += 1;
        }
        if !eligible(&span) || next >= links.len() || links[next].start >= span.range.end {
            out.push(span);
            continue;
        }
        // Split the span at every link boundary inside it. Inside a link,
        // everything shows as typed: delimiters, escapes and entities too.
        // An entity cut by a link boundary shows its source as well.
        let outside = match span.kind {
            SpanKind::Replaced(_) => SpanKind::Text,
            ref k => k.clone(),
        };
        let cut = links[next].start > span.range.start || links[next].end < span.range.end;
        let outside = if cut { outside } else { span.kind.clone() };
        let mut at = span.range.start;
        let mut k = next;
        while k < links.len() && links[k].start < span.range.end {
            let link = links[k].start.max(span.range.start)..links[k].end.min(span.range.end);
            if at < link.start {
                out.push(Span {
                    range: at..link.start,
                    kind: outside.clone(),
                    ..span.clone()
                });
            }
            // A link shows its text literally: no emphasis inside it.
            let mut style = span.style;
            for inline in [Style::EMPHASIS, Style::STRONG, Style::STRIKE] {
                style.remove(inline);
            }
            style.insert(Style::LINK);
            let piece = Span {
                range: link.clone(),
                kind: SpanKind::Text,
                style,
                ..span.clone()
            };
            // Pieces of one link (from text split at `*`, entities, ...) merge.
            match out.last_mut() {
                Some(prev)
                    if prev.kind == SpanKind::Text
                        && prev.style == piece.style
                        && prev.heading == piece.heading
                        && prev.range.end == piece.range.start =>
                {
                    prev.range.end = piece.range.end;
                }
                _ => out.push(piece),
            }
            at = link.end;
            k += 1;
        }
        if at < span.range.end {
            out.push(Span {
                range: at..span.range.end,
                kind: outside,
                ..span
            });
        }
    }
    *map = SourceMap::from_spans(out);
}

/// Autolink literals within `src[range]`, as absolute byte ranges.
pub(crate) fn find(src: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let text = &src[range.clone()];
    if !text.contains(['.', '@']) {
        return Vec::new();
    }
    let b = text.as_bytes();
    let mut links = Vec::new();
    let mut i = 0;
    // No link may start inside the previous one.
    let mut floor = 0;
    while i < b.len() {
        let abs = range.start + i;
        // `www.` is literal; only URL schemes are case-insensitive.
        let found = if b[i..].starts_with(b"www.") && preceded_ok(src, abs) {
            domain_end(b, i).map(|end| path_end(b, end))
        } else if let Some(scheme) = [&b"https://"[..], b"http://", b"ftp://"]
            .into_iter()
            .find(|s| starts_with_ci(&b[i..], s))
            && preceded_ok(src, abs)
        {
            domain_end(b, i + scheme.len()).map(|end| path_end(b, end))
        } else if b[i] == b'@' {
            email(b, i, floor).map(|(start, end)| {
                // Email links start at the local part, before `i`.
                i = start;
                end
            })
        } else {
            None
        };
        match found {
            Some(end) => {
                let start = i;
                let end = if b[start..end].contains(&b'@') && !is_url(&b[start..end]) {
                    end
                } else {
                    trim_trailing(b, start, end)
                };
                if end > start {
                    links.push(range.start + start..range.start + end);
                    floor = end;
                    i = end;
                    continue;
                }
                i += 1;
            }
            None => i += 1,
        }
        while i < b.len() && !text.is_char_boundary(i) {
            i += 1;
        }
    }
    links
}

fn is_url(b: &[u8]) -> bool {
    [&b"www."[..], b"http://", b"https://", b"ftp://"]
        .iter()
        .any(|p| starts_with_ci(b, p))
}

fn starts_with_ci(b: &[u8], prefix: &[u8]) -> bool {
    b.len() >= prefix.len() && b[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// Autolinks start at a line start, after whitespace, or after `*_~(`.
fn preceded_ok(src: &str, at: usize) -> bool {
    match src[..at].chars().next_back() {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, '*' | '_' | '~' | '('),
    }
}

fn is_domain_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// End of a valid domain starting at `start`: segments of alphanumerics,
/// `_` and `-` separated by periods, at least one period, and no `_` in the
/// last two segments.
fn domain_end(b: &[u8], start: usize) -> Option<usize> {
    let mut end = start;
    let mut segments: Vec<Range<usize>> = Vec::new();
    loop {
        let seg_start = end;
        while end < b.len() && is_domain_char(b[end]) {
            end += 1;
        }
        if end == seg_start {
            break;
        }
        segments.push(seg_start..end);
        if end + 1 < b.len() && b[end] == b'.' && is_domain_char(b[end + 1]) {
            end += 1;
        } else {
            break;
        }
    }
    if segments.len() < 2 {
        return None;
    }
    let tail_ok = segments[segments.len() - 2..]
        .iter()
        .all(|s| !b[s.clone()].contains(&b'_'));
    tail_ok.then_some(end)
}

/// A link's path runs until whitespace or `<`.
fn path_end(b: &[u8], from: usize) -> usize {
    let mut end = from;
    while end < b.len() && !b[end].is_ascii_whitespace() && b[end] != b'<' {
        end += 1;
    }
    end
}

/// Trailing punctuation, unbalanced `)` and an entity-like `&name;` at the
/// end aren't part of the link.
fn trim_trailing(b: &[u8], start: usize, mut end: usize) -> usize {
    loop {
        let Some(&last) = b[start..end].last() else {
            return end;
        };
        match last {
            b'?' | b'!' | b'.' | b',' | b':' | b'*' | b'_' | b'~' => end -= 1,
            b')' => {
                let link = &b[start..end];
                let open = link.iter().filter(|&&c| c == b'(').count();
                let close = link.iter().filter(|&&c| c == b')').count();
                if close > open {
                    end -= 1;
                } else {
                    return end;
                }
            }
            b';' => {
                let link = &b[start..end - 1];
                let name = link
                    .iter()
                    .rev()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .count();
                if name > 0 && link.len() > name && link[link.len() - name - 1] == b'&' {
                    end -= name + 2;
                } else {
                    return end;
                }
            }
            _ => return end,
        }
    }
}

/// An email address around the `@` at `at`: `[A-Za-z0-9._+-]+@` and a
/// domain with at least one period, not ending in `-` or `_` (a trailing
/// `.` is dropped). Returns (start, end).
fn email(b: &[u8], at: usize, floor: usize) -> Option<(usize, usize)> {
    let is_local = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'+');
    let mut start = at;
    while start > floor && is_local(b[start - 1]) {
        start -= 1;
    }
    if start == at {
        return None;
    }
    let mut end = at + 1;
    let mut dots = 0;
    while end < b.len()
        && (is_domain_char(b[end])
            || (b[end] == b'.' && end + 1 < b.len() && is_domain_char(b[end + 1])))
    {
        if b[end] == b'.' {
            dots += 1;
        }
        end += 1;
    }
    if dots == 0 || end == at + 1 || matches!(b[end - 1], b'-' | b'_') {
        return None;
    }
    // `mailto:` and `xmpp:` belong to the link; xmpp may add one resource.
    let before = &b[floor..start];
    if before.len() >= 7 && before[before.len() - 7..].eq_ignore_ascii_case(b"mailto:") {
        start -= 7;
    } else if before.len() >= 5 && before[before.len() - 5..].eq_ignore_ascii_case(b"xmpp:") {
        start -= 5;
        if end < b.len() && b[end] == b'/' {
            let resource = b[end + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, b'@' | b'.'))
                .count();
            if resource > 0 {
                end += 1 + resource;
                // A trailing period ends the sentence, not the resource.
                while b[end - 1] == b'.' {
                    end -= 1;
                }
            }
        }
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(text: &str) -> Vec<&str> {
        find(text, 0..text.len())
            .into_iter()
            .map(|r| &text[r])
            .collect()
    }

    #[test]
    fn www_and_scheme_links() {
        assert_eq!(
            links("see www.commonmark.org/help for more"),
            vec!["www.commonmark.org/help"]
        );
        assert_eq!(
            links("Visit www.commonmark.org."),
            vec!["www.commonmark.org"]
        );
        assert_eq!(
            links("https://example.com/a?b=c, then"),
            vec!["https://example.com/a?b=c"]
        );
        assert_eq!(links("xwww.example.com"), Vec::<&str>::new());
        assert_eq!(
            links("(www.google.com/search?q=Markup+(business))"),
            vec!["www.google.com/search?q=Markup+(business)"]
        );
        assert_eq!(
            links("www.google.com/search?q=Markup+(business)))"),
            vec!["www.google.com/search?q=Markup+(business)"]
        );
        assert_eq!(
            links("www.google.com/search?q=commonmark&hl;"),
            vec!["www.google.com/search?q=commonmark"]
        );
        assert_eq!(
            links("www.commonmark.org/he<lp"),
            vec!["www.commonmark.org/he"]
        );
        assert_eq!(links("ftp://foo.bar.baz"), vec!["ftp://foo.bar.baz"]);
        assert_eq!(links("www.a_b.cd.com"), vec!["www.a_b.cd.com"]);
        assert_eq!(links("www.a_b.c_d.com"), Vec::<&str>::new());
        assert_eq!(links("www.a.b_c.d"), Vec::<&str>::new());
        // Regression for #16: www. is lowercase-only; schemes aren't.
        assert_eq!(links("WWW.example.com"), Vec::<&str>::new());
        assert_eq!(links("HTTPS://example.com"), vec!["HTTPS://example.com"]);
    }

    #[test]
    fn emails() {
        assert_eq!(links("foo@bar.baz"), vec!["foo@bar.baz"]);
        assert_eq!(links("hello@mail+xyz.example isn't"), Vec::<&str>::new());
        assert_eq!(
            links("hello+xyz@mail.example"),
            vec!["hello+xyz@mail.example"]
        );
        assert_eq!(links("a.b-c_d@a.b."), vec!["a.b-c_d@a.b"]);
        assert_eq!(links("a.b-c_d@a.b-"), Vec::<&str>::new());
        assert_eq!(links("a.b-c_d@a.b_"), Vec::<&str>::new());
        // Regression for #16: the scheme is part of the link.
        assert_eq!(links("mailto:foo@bar.baz"), vec!["mailto:foo@bar.baz"]);
        assert_eq!(links("xmpp:foo@bar.baz/txt"), vec!["xmpp:foo@bar.baz/txt"]);
        assert_eq!(
            links("xmpp:foo@bar.baz/txt@bin.com."),
            vec!["xmpp:foo@bar.baz/txt@bin.com"]
        );
    }

    #[test]
    fn urls_keep_emphasis_and_entities_inside_them() {
        // Regression for #16: scanned on raw text, as cmark-gfm does.
        use crate::{GfmParser, MarkdownParser};
        let linked = |src: &str| -> Vec<String> {
            let out = GfmParser.parse(src);
            let mut runs: Vec<String> = Vec::new();
            let mut prev = false;
            for s in out.map.iter() {
                let link = s.kind == SpanKind::Text && s.style.contains(Style::LINK);
                if link && prev {
                    runs.last_mut().unwrap().push_str(&src[s.range]);
                } else if link {
                    runs.push(src[s.range].to_owned());
                }
                prev = link;
            }
            runs
        };
        assert_eq!(
            linked("https://example.com/a*b*c\n"),
            vec!["https://example.com/a*b*c"]
        );
        assert_eq!(
            linked("https://example.com?a=1&amp;b=2\n"),
            vec!["https://example.com?a=1&amp;b=2"]
        );
        // Emphasis around a URL stays emphasis.
        let out = GfmParser.parse("*see www.x.com*\n");
        out.map.validate(16).unwrap();
        assert_eq!(linked("*see www.x.com*\n"), vec!["www.x.com"]);
    }
}
