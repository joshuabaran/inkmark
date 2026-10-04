//! `$...$` and `$$...$$` in ordinary text. The source bytes stay as typed.
//! Delimiters become syntax and the body is marked so the live pane can
//! draw a formula there. Code, code blocks, and HTML are left alone.
//! A backslash is already an escape, so `\(...\)` and `\[...\]` are not
//! formulas.

use std::ops::Range;

use crate::map::{SourceMap, Span, SpanKind, Style, Syntax};

struct Formula {
    open: Range<usize>,
    body: Range<usize>,
    close: Range<usize>,
}

pub(crate) fn mark(src: &str, map: &mut SourceMap) {
    let spans: Vec<Span> = map.iter().collect();
    let formulas = find(src, &spans);
    if formulas.is_empty() {
        return;
    }
    let mut out = Vec::with_capacity(spans.len() + formulas.len() * 2);
    let mut fi = 0;
    for span in spans {
        while fi < formulas.len() && formulas[fi].close.end <= span.range.start {
            fi += 1;
        }
        if fi >= formulas.len() || formulas[fi].open.start >= span.range.end {
            out.push(span);
        } else {
            out.extend(cut_span(span, &formulas[fi..]));
        }
    }
    *map = SourceMap::from_spans(out);
}

fn allowed(span: &Span) -> bool {
    !span.style.contains(Style::CODE)
        && !span.style.contains(Style::CODE_BLOCK)
        && !span.style.contains(Style::HTML)
}

/// The `$` at `i` is the character an escape produced, so it is not an opener.
fn escaped_before(src: &str, spans: &[Span], si: usize, i: usize) -> bool {
    if i == 0 || src.as_bytes()[i - 1] != b'\\' {
        return false;
    }
    let at = i - 1;
    let span = if spans[si].range.start <= at {
        &spans[si]
    } else if si > 0 {
        &spans[si - 1]
    } else {
        return false;
    };
    span.range.start <= at
        && at < span.range.end
        && matches!(span.kind, SpanKind::Syntax(Syntax::Escape))
}

fn span_index(spans: &[Span], mut si: usize, i: usize) -> Option<usize> {
    while si < spans.len() && spans[si].range.end <= i {
        si += 1;
    }
    (si < spans.len() && spans[si].range.start <= i && i < spans[si].range.end).then_some(si)
}

fn covers_only_allowed(spans: &[Span], range: Range<usize>, hint: usize) -> bool {
    let mut at = range.start;
    for span in spans.iter().skip(hint) {
        if span.range.end <= range.start {
            continue;
        }
        if span.range.start >= range.end {
            break;
        }
        if span.range.start > at || !allowed(span) {
            return false;
        }
        at = at.max(span.range.end);
        if at >= range.end {
            return true;
        }
    }
    at >= range.end
}

fn find(src: &str, spans: &[Span]) -> Vec<Formula> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut si = 0;
    let mut i = 0;
    while i < bytes.len() {
        while si < spans.len() && spans[si].range.end <= i {
            si += 1;
        }
        if si >= spans.len() || spans[si].range.start > i {
            break;
        }
        let span = &spans[si];
        if !allowed(span) {
            i = span.range.end;
            continue;
        }
        if !matches!(span.kind, SpanKind::Text)
            || bytes[i] != b'$'
            || escaped_before(src, spans, si, i)
        {
            i += 1;
            continue;
        }
        let display = i + 1 < bytes.len() && bytes[i + 1] == b'$';
        let formula = if display {
            display_at(src, spans, si, i)
        } else {
            inline_at(src, spans, si, i)
        };
        if let Some(formula) = formula {
            i = formula.close.end;
            out.push(formula);
        } else {
            i += 1;
        }
    }
    out
}

fn display_at(src: &str, spans: &[Span], si: usize, i: usize) -> Option<Formula> {
    let bytes = src.as_bytes();
    let mut j = i + 2;
    while j + 1 < bytes.len() {
        if bytes[j] == b'\n' && bytes[j + 1] == b'\n' {
            return None;
        }
        if bytes[j] == b'$' && bytes[j + 1] == b'$' {
            let sj = span_index(spans, si, j)?;
            if escaped_before(src, spans, sj, j) {
                j += 1;
                continue;
            }
            let body = i + 2..j;
            let whole = i..j + 2;
            if body_has_text(src, body.clone()) && covers_only_allowed(spans, whole, si) {
                return Some(Formula {
                    open: i..i + 2,
                    body,
                    close: j..j + 2,
                });
            }
            return None;
        }
        j += 1;
    }
    None
}

fn inline_at(src: &str, spans: &[Span], si: usize, i: usize) -> Option<Formula> {
    let bytes = src.as_bytes();
    let after = src[i + 1..].chars().next()?;
    if after.is_whitespace() {
        return None;
    }
    let mut j = i + 1;
    while j < bytes.len() {
        if bytes[j] == b'\n' {
            return None;
        }
        if bytes[j] == b'$' {
            let sj = span_index(spans, si, j)?;
            if escaped_before(src, spans, sj, j) {
                j += 1;
                continue;
            }
            let preceded_by_space = src[..j]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            let doubled = j + 1 < bytes.len() && bytes[j + 1] == b'$';
            if !preceded_by_space
                && !doubled
                && j > i + 1
                && covers_only_allowed(spans, i..j + 1, si)
            {
                return Some(Formula {
                    open: i..i + 1,
                    body: i + 1..j,
                    close: j..j + 1,
                });
            }
            return None;
        }
        j += 1;
    }
    None
}

fn body_has_text(src: &str, body: Range<usize>) -> bool {
    src[body].chars().any(|c| !c.is_whitespace())
}

enum Part {
    Keep,
    Body,
    Delim,
}

fn cut_span(span: Span, formulas: &[Formula]) -> Vec<Span> {
    let mut out = Vec::new();
    let mut at = span.range.start;
    let end = span.range.end;
    for formula in formulas {
        if formula.close.end <= at {
            continue;
        }
        if formula.open.start >= end {
            break;
        }
        if at < formula.open.start {
            let to = formula.open.start.min(end);
            out.push(piece(&span, at..to, Part::Keep));
            at = to;
        }
        for (region, part) in [
            (formula.open.clone(), Part::Delim),
            (formula.body.clone(), Part::Body),
            (formula.close.clone(), Part::Delim),
        ] {
            let a = region.start.max(at);
            let b = region.end.min(end);
            if a < b {
                out.push(piece(&span, a..b, part));
                at = b;
            }
        }
        if at >= end {
            break;
        }
    }
    if at < end {
        out.push(piece(&span, at..end, Part::Keep));
    }
    out
}

fn piece(span: &Span, range: Range<usize>, part: Part) -> Span {
    let mut style = span.style;
    let kind = match part {
        Part::Keep => span.kind.clone(),
        Part::Body => {
            style.insert(Style::MATH);
            span.kind.clone()
        }
        Part::Delim => {
            style.insert(Style::MATH);
            SpanKind::Syntax(Syntax::Delimiter)
        }
    };
    Span {
        range,
        kind,
        style,
        heading: span.heading,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GfmParser, MarkdownParser, SpanKind, Style};

    fn marked(src: &str) -> Vec<Span> {
        GfmParser
            .parse(src)
            .map
            .iter()
            .filter(|s| s.style.contains(Style::MATH))
            .collect()
    }

    #[test]
    fn inline_and_display_keep_their_bytes() {
        let src = "Energy $E=mc^2$ today.\n\n$$\\frac{1}{2}$$\n";
        let spans = marked(src);
        assert_eq!(spans.len(), 6);
        assert!(matches!(spans[0].kind, SpanKind::Syntax(_)));
        assert_eq!(&src[spans[1].range.clone()], "E=mc^2");
        assert!(!spans[1].style.contains(Style::CODE));
        assert!(matches!(spans[3].kind, SpanKind::Syntax(_)));
        assert_eq!(&src[spans[3].range.clone()], "$$");
        assert_eq!(&src[spans[4].range.clone()], "\\frac{1}{2}");
        let again = GfmParser.parse(src);
        again.map.validate(src.len()).unwrap();
        let plain: Vec<_> = again
            .map
            .iter()
            .filter(|s| !s.style.contains(Style::MATH))
            .map(|s| src[s.range.clone()].to_owned())
            .collect();
        assert!(plain.iter().any(|s| s.contains("Energy")));
        assert!(plain.iter().any(|s| s.contains("today")));
    }

    #[test]
    fn a_display_formula_may_break_a_line() {
        let src = "$$\n\\frac{1}{2}\n$$\n";
        let spans = marked(src);
        assert!(spans.len() >= 3);
        assert_eq!(&src[spans[0].range.clone()], "$$");
        assert!(spans.iter().any(|s| src[s.range.clone()].contains("frac")));
        GfmParser.parse(src).map.validate(src.len()).unwrap();
    }

    #[test]
    fn code_escapes_and_currency_stay_text() {
        for src in [
            "use `$x$` here\n",
            "cost is \\$5 today\n",
            "pay $5 and $10 now\n",
            "hello $.;'there\n",
            "$$\n\n$$\n",
            "```\n$x$\n```\n",
            "\\(a\\) and \\[b\\]\n",
        ] {
            assert!(marked(src).is_empty(), "marked {src:?}");
            GfmParser.parse(src).map.validate(src.len()).unwrap();
        }
    }
}
