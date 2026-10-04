//! Structural selection for the code pane.
//!
//! Select word uses the same ranges as a double-click. Select paragraph
//! takes the innermost leaf block, or the blank-line span when the caret
//! is between blocks or the parse has no blocks. A fence or link jump
//! places the caret on the other side.

use std::ops::Range;

use inkmark_buffer::{Document, Selection};
use inkmark_parse::{Block, BlockKind, BlockTree, ParseOutput, SpanKind, Syntax};

use crate::motion;

/// Byte the command should read. A selection uses its last byte, so a
/// caret sitting on the exclusive end stays with what was just selected.
fn probe(doc: &Document, selection: Selection) -> usize {
    let range = selection.range();
    let at = if range.is_empty() {
        selection.head
    } else {
        range.end - 1
    };
    at.min(doc.len())
}

/// The word under the caret, including a whitespace or punctuation run.
pub(crate) fn word_range(doc: &Document, selection: Selection) -> Range<usize> {
    motion::word_at(doc, probe(doc, selection))
}

/// The paragraph (or other leaf block) around the caret.
pub(crate) fn paragraph_range(
    doc: &Document,
    selection: Selection,
    parse: Option<&ParseOutput>,
) -> Range<usize> {
    // A caret past the last byte still belongs to that byte.
    let mut at = probe(doc, selection);
    if at == doc.len() && at > 0 {
        at -= 1;
    }
    let Some(parse) = parse.filter(|p| p.map.len() == doc.len() && !p.blocks.is_empty()) else {
        return blank_paragraph(doc, at);
    };
    if let Some(range) = leaf_containing(&parse.blocks, at) {
        return range;
    }
    // The caret sits on the exclusive end of a block, on that same line.
    // A caret at the start of the next line is the gap, not this block.
    if at > 0
        && same_line(doc, at - 1, at)
        && let Some(range) = leaf_containing(&parse.blocks, at - 1)
    {
        return range;
    }
    // A list marker or quote prefix on the same line belongs to the leaf
    // that follows it, not to the blank-line fallback.
    if let Some(leaf) = parse.blocks.leaves_from(at).next() {
        let range = leaf.block.range;
        if range.start > at && range.start < doc.len() && same_line(doc, at, range.start) {
            return range;
        }
    }
    blank_paragraph(doc, at)
}

fn leaf_containing(blocks: &BlockTree, offset: usize) -> Option<Range<usize>> {
    let leaf = blocks.leaves_from(offset).next()?;
    let range = leaf.block.range;
    range.contains(&offset).then_some(range)
}

fn same_line(doc: &Document, a: usize, b: usize) -> bool {
    doc.byte_to_line(a.min(doc.len())) == doc.byte_to_line(b.min(doc.len()))
}

/// Lines through the next blank line. A blank caret selects the blank run.
fn blank_paragraph(doc: &Document, offset: usize) -> Range<usize> {
    if doc.is_empty() {
        return 0..0;
    }
    let offset = offset.min(doc.len());
    let line = if offset == doc.len() {
        doc.byte_to_line(offset - 1)
    } else {
        doc.byte_to_line(offset)
    };
    let blank = |line: usize| doc.slice(doc.line_range(line)).trim().is_empty();
    let is_blank = blank(line);
    let mut start = line;
    while start > 0 && blank(start - 1) == is_blank {
        start -= 1;
    }
    let last = doc.line_count() - 1;
    let mut end = line;
    while end < last && blank(end + 1) == is_blank {
        end += 1;
    }
    let start_byte = doc.line_to_byte(start);
    let end_byte = if end + 1 < doc.line_count() {
        doc.line_to_byte(end + 1)
    } else {
        doc.len()
    };
    start_byte..end_byte
}

/// Where a fence or link-bracket jump lands, or `None` when nothing matches.
pub(crate) fn bracket_target(
    doc: &Document,
    selection: Selection,
    parse: Option<&ParseOutput>,
) -> Option<usize> {
    let parse = parse.filter(|p| p.map.len() == doc.len())?;
    let head = selection.head.min(doc.len());
    // Inside a fenced block the fences win, including when the body has
    // brackets. An indented code block has no fence and falls through.
    if let Some(target) = fence_target(head, parse) {
        return target;
    }
    link_target(doc, head, parse)
}

/// `None` outside a fenced block. `Some(None)` when the block has no
/// second fence to land on.
fn fence_target(head: usize, parse: &ParseOutput) -> Option<Option<usize>> {
    let (block, probe) = fenced_probe(&parse.blocks, head)?;
    let mut fences = Vec::new();
    for span in parse.map.spans_in(block.range) {
        if span.kind == SpanKind::Syntax(Syntax::Fence) {
            fences.push(span.range);
        }
    }
    let (Some(open), Some(close)) = (fences.first(), fences.last()) else {
        return Some(None);
    };
    if fences.len() < 2 || open == close {
        return Some(None);
    }
    let on_close = close.contains(&probe);
    let dest = if on_close { open.start } else { close.start };
    Some(Some(dest))
}

fn fenced_probe(blocks: &BlockTree, head: usize) -> Option<(Block, usize)> {
    if let Some(block) = fenced_at(blocks, head) {
        return Some((block, head));
    }
    if head > 0
        && let Some(block) = fenced_at(blocks, head - 1)
    {
        return Some((block, head - 1));
    }
    None
}

fn fenced_at(blocks: &BlockTree, offset: usize) -> Option<Block> {
    let leaf = blocks.leaves_from(offset).next()?;
    let block = leaf.block;
    (block.range.contains(&offset) && matches!(block.kind, BlockKind::CodeBlock { fenced: true }))
        .then_some(block)
}

/// The bracket at the caret, or the one just before it.
fn caret_bracket(doc: &Document, head: usize) -> Option<usize> {
    let head = head.min(doc.len());
    if is_bracket_at(doc, head) {
        return Some(head);
    }
    if head == 0 {
        return None;
    }
    let prev = doc.prev_char_boundary(head);
    (prev < head && is_bracket_at(doc, prev)).then_some(prev)
}

fn is_bracket_at(doc: &Document, offset: usize) -> bool {
    char_at(doc, offset).is_some_and(|ch| matches!(ch, '[' | ']' | '(' | ')' | '<' | '>'))
}

fn char_at(doc: &Document, offset: usize) -> Option<char> {
    if offset >= doc.len() || !doc.is_char_boundary(offset) {
        return None;
    }
    let mut end = (offset + 4).min(doc.len());
    while !doc.is_char_boundary(end) {
        end += 1;
    }
    doc.slice(offset..end).chars().next()
}

struct LinkPair {
    open: Range<usize>,
    close: Range<usize>,
}

fn link_pairs(doc: &Document, parse: &ParseOutput) -> Vec<LinkPair> {
    let mut stack = Vec::new();
    let mut pairs = Vec::new();
    for span in parse.map.iter() {
        if span.kind != SpanKind::Syntax(Syntax::LinkMarkup) {
            continue;
        }
        let text = doc.slice(span.range.clone());
        if text.starts_with(']') || text.starts_with('>') {
            if let Some(open) = stack.pop() {
                pairs.push(LinkPair {
                    open,
                    close: span.range,
                });
            }
        } else if (text.contains('[') || text.contains('<'))
            && (text.contains(']') || text.contains('>'))
        {
            // `[](u)` and `![](img.png)` arrive as one span. Pushing that
            // span leaves it on the stack, and the closer of an image whose
            // alt holds the empty link then binds to the empty link.
            pairs.push(LinkPair {
                open: span.range.clone(),
                close: span.range,
            });
        } else if text.contains('[') || text.contains('<') {
            stack.push(span.range);
        }
    }
    pairs
}

fn link_target(doc: &Document, head: usize, parse: &ParseOutput) -> Option<usize> {
    let at = caret_bracket(doc, head)?;
    let pairs = link_pairs(doc, parse);
    let pair = pairs
        .iter()
        .find(|pair| contains(&pair.open, at) || contains(&pair.close, at))?;
    let brackets = structural_brackets(doc, &pair.open, &pair.close);
    partner(&brackets, at)
}

fn contains(range: &Range<usize>, offset: usize) -> bool {
    range.contains(&offset)
}

/// Brackets a jump can land on, in source order. `[` `]` `<` `>` stay, and
/// so do the `(` `)` that delimit a destination. A parenthesis inside the
/// URL or the title stays put, including one written `\(` or `\)`.
fn structural_brackets(
    doc: &Document,
    open: &Range<usize>,
    close: &Range<usize>,
) -> Vec<(usize, char)> {
    let mut brackets = Vec::new();
    collect_brackets(&mut brackets, doc, open);
    if open != close {
        collect_brackets(&mut brackets, doc, close);
    }
    let dest = destination_parens(doc, close);
    brackets.retain(|(at, ch)| match ch {
        '(' => dest.is_some_and(|(open_at, _)| *at == open_at),
        ')' => dest.is_some_and(|(_, close_at)| *at == close_at),
        _ => true,
    });
    brackets
}

fn collect_brackets(out: &mut Vec<(usize, char)>, doc: &Document, range: &Range<usize>) {
    let mut i = range.start;
    for ch in doc.slice(range.clone()).chars() {
        if matches!(ch, '[' | ']' | '(' | ')' | '<' | '>') && !escaped(doc, i) {
            out.push((i, ch));
        }
        i += ch.len_utf8();
    }
}

/// The `(` just after the label's `]`, and the last unescaped `)` in the
/// closing span. That last `)` is the one that ends the destination.
fn destination_parens(doc: &Document, close: &Range<usize>) -> Option<(usize, usize)> {
    let bracket = first_unescaped(doc, close, ']')?;
    let after = bracket + ']'.len_utf8();
    if after >= close.end || escaped(doc, after) || char_at(doc, after) != Some('(') {
        return None;
    }
    let end = last_unescaped(doc, close, ')')?;
    (end > after).then_some((after, end))
}

fn first_unescaped(doc: &Document, range: &Range<usize>, want: char) -> Option<usize> {
    let mut i = range.start;
    for ch in doc.slice(range.clone()).chars() {
        if ch == want && !escaped(doc, i) {
            return Some(i);
        }
        i += ch.len_utf8();
    }
    None
}

fn last_unescaped(doc: &Document, range: &Range<usize>, want: char) -> Option<usize> {
    let mut i = range.start;
    let mut found = None;
    for ch in doc.slice(range.clone()).chars() {
        if ch == want && !escaped(doc, i) {
            found = Some(i);
        }
        i += ch.len_utf8();
    }
    found
}

/// A bracket written `\(`, `\)`, and the same for the other brackets.
fn escaped(doc: &Document, offset: usize) -> bool {
    let mut at = offset;
    let mut slashes = 0u32;
    while at > 0 {
        let prev = doc.prev_char_boundary(at);
        if char_at(doc, prev) != Some('\\') {
            break;
        }
        slashes += 1;
        at = prev;
    }
    slashes % 2 == 1
}

fn partner(brackets: &[(usize, char)], at: usize) -> Option<usize> {
    let mut match_of = vec![None; brackets.len()];
    let mut stack = Vec::new();
    for (i, &(_, ch)) in brackets.iter().enumerate() {
        match ch {
            '[' | '(' | '<' => stack.push(i),
            ']' | ')' | '>' => {
                let want = match ch {
                    ']' => '[',
                    ')' => '(',
                    _ => '<',
                };
                if let Some(j) = stack.last().copied()
                    && brackets[j].1 == want
                {
                    stack.pop();
                    match_of[i] = Some(brackets[j].0);
                    match_of[j] = Some(brackets[i].0);
                }
            }
            _ => {}
        }
    }
    let i = brackets.iter().position(|(off, _)| *off == at)?;
    match_of[i]
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkmark_parse::{GfmParser, MarkdownParser};

    fn parsed(src: &str) -> (Document, ParseOutput) {
        (Document::from_text(src), GfmParser.parse(src))
    }

    fn paragraph_at(src: &str, at: usize) -> String {
        let (doc, parse) = parsed(src);
        let range = paragraph_range(&doc, Selection::caret(at), Some(&parse));
        src[range].to_owned()
    }

    fn jump_at(src: &str, at: usize) -> Option<usize> {
        let (doc, parse) = parsed(src);
        bracket_target(&doc, Selection::caret(at), Some(&parse))
    }

    #[test]
    fn select_word_matches_a_double_click_and_stays_put() {
        let doc = Document::from_text("hello, big  world");
        assert_eq!(word_range(&doc, Selection::caret(1)), 0..5);
        assert_eq!(word_range(&doc, Selection::caret(5)), 5..6);
        assert_eq!(word_range(&doc, Selection::caret(7)), 7..10);
        assert_eq!(word_range(&doc, Selection::caret(10)), 10..12);
        // The caret at the exclusive end of the selection is still that word.
        let selected = Selection { anchor: 0, head: 5 };
        assert_eq!(word_range(&doc, selected), 0..5);
        assert_eq!(word_range(&doc, Selection::caret(doc.len())), 12..17);

        let cafe = Document::from_text("caf\u{e9} time");
        let word = word_range(&cafe, Selection::caret(0));
        assert_eq!(&cafe.slice(word.clone())[..], "caf\u{e9}");
        let again = Selection {
            anchor: word.start,
            head: word.end,
        };
        assert_eq!(word_range(&cafe, again), word);
    }

    #[test]
    fn select_paragraph_uses_the_leaf_and_falls_back_between_blocks() {
        let src = "hello\n\nworld\n";
        // A paragraph's range includes the newline that ends it.
        assert_eq!(paragraph_at(src, 1), "hello\n");
        assert_eq!(paragraph_at(src, 5), "hello\n");
        // The blank line between them.
        assert_eq!(paragraph_at(src, 6), "\n");
        assert_eq!(paragraph_at(src, src.find("world").unwrap()), "world\n");
        // End of the file stays with the last paragraph.
        assert_eq!(paragraph_at(src, src.len()), "world\n");

        let src = "- one *em*\n  more\n- two\n";
        assert_eq!(
            paragraph_at(src, src.find("em").unwrap()),
            "one *em*\n  more"
        );
        assert_eq!(paragraph_at(src, 0), "one *em*\n  more");
        assert_eq!(paragraph_at(src, src.find("two").unwrap()), "two");

        let src = "# Title\n\nbody\n";
        let heading = paragraph_at(src, 2);
        assert!(heading.contains("Title"), "{heading:?}");
        assert!(!heading.contains("body"), "{heading:?}");

        let src = "```\ncode\n```\n\nnext\n";
        let block = paragraph_at(src, src.find("code").unwrap());
        assert!(block.contains("```") && block.contains("code"), "{block:?}");
        assert!(!block.contains("next"), "{block:?}");

        let src = "    indented\n";
        let block = paragraph_at(src, 4);
        assert!(block.contains("indented"), "{block:?}");

        let src = "before\n\n| a |\n| - |\n| b |\n\nafter\n";
        let table = paragraph_at(src, src.find("| a |").unwrap());
        assert!(
            table.contains("| a |") && table.contains("| b |"),
            "{table:?}"
        );
        assert!(
            !table.contains("before") && !table.contains("after"),
            "{table:?}"
        );

        let src = "> quoted\n";
        assert_eq!(paragraph_at(src, src.find('q').unwrap()), "quoted\n");
        assert_eq!(paragraph_at(src, 0), "quoted\n");

        let src = "alpha\n\nbeta\n";
        let (doc, _) = parsed(src);
        let stale = paragraph_range(&doc, Selection::caret(1), None);
        assert_eq!(&src[stale], "alpha\n");
        let unparsed = ParseOutput::unparsed(src.len());
        let stale = paragraph_range(&doc, Selection::caret(1), Some(&unparsed));
        assert_eq!(&src[stale], "alpha\n");
        let blank = paragraph_range(&doc, Selection::caret(src.find("\n\n").unwrap() + 1), None);
        assert_eq!(&src[blank], "\n");
    }

    #[test]
    fn fences_jump_and_indented_code_does_not() {
        let src = "```rust\ncode\n```\n";
        let open = src.find("```").unwrap();
        let close = src.rfind("```").unwrap();
        let body = src.find("code").unwrap();
        assert_eq!(jump_at(src, open), Some(close));
        assert_eq!(jump_at(src, open + 3), Some(close));
        assert_eq!(jump_at(src, close), Some(open));
        assert_eq!(jump_at(src, close + 2), Some(open));
        assert_eq!(jump_at(src, body), Some(close));
        // A bracket in the body still jumps to the fence.
        let src = "```\nlet a = [1]\n```\n";
        let bracket = src.find('[').unwrap();
        assert_eq!(jump_at(src, bracket), Some(src.rfind("```").unwrap()));

        assert_eq!(jump_at("```\ncode\n", 0), None);
        assert_eq!(jump_at("    code\n", 4), None);
        assert_eq!(
            jump_at("~~~\ncode\n~~~\n", 0),
            Some("~~~\ncode\n~~~\n".rfind("~~~").unwrap())
        );
    }

    #[test]
    fn link_brackets_jump_and_other_brackets_stay() {
        let src = "see [a link](http://x.com) here";
        let open = src.find('[').unwrap();
        let close = src.find(']').unwrap();
        let paren = src.find('(').unwrap();
        let end = src.find(')').unwrap();
        assert_eq!(jump_at(src, open), Some(close));
        assert_eq!(jump_at(src, close), Some(open));
        assert_eq!(jump_at(src, paren), Some(end));
        assert_eq!(jump_at(src, end), Some(paren));
        // Just after the shortcut's `]`, the byte before is the bracket.
        let src = "see [text] more\n\n[text]: http://x.com\n";
        let close = src.find(']').unwrap();
        assert_eq!(jump_at(src, close + 1), Some(src.find('[').unwrap()));
        assert_eq!(jump_at(src, src.rfind('[').unwrap()), None);

        let src = "![alt](img.png)";
        assert_eq!(jump_at(src, 0), None);
        assert_eq!(jump_at(src, 1), Some(src.find(']').unwrap()));
        assert_eq!(jump_at(src, src.find(']').unwrap()), Some(1));

        let src = "[a](u) and [b](v)";
        let second = src.rfind('[').unwrap();
        assert_eq!(jump_at(src, 0), Some(src.find(']').unwrap()));
        assert_eq!(jump_at(src, second), Some(src.rfind(']').unwrap()));

        let src = "[![a](i.png)](u)";
        let image = src.find("![").unwrap() + 1;
        assert_eq!(jump_at(src, 0), Some(src.rfind(']').unwrap()));
        assert_eq!(jump_at(src, image), Some(src.find(']').unwrap()));

        let src = "[a](http://x.com \"t(y)\")";
        let open_paren = src.find('(').unwrap();
        let title_paren = src.find("(y)").unwrap();
        assert_eq!(jump_at(src, open_paren), Some(src.rfind(')').unwrap()));
        assert_eq!(jump_at(src, title_paren), None);
        assert_eq!(jump_at(src, src.find(')').unwrap()), None);
        assert_eq!(jump_at(src, src.rfind(')').unwrap()), Some(open_paren));

        let src = "see [foo] and [bar](http://x.com)\n\n[foo]: http://x.com\n";
        let def = src.rfind('[').unwrap();
        assert_eq!(
            jump_at(src, src.find('[').unwrap()),
            Some(src.find(']').unwrap())
        );
        assert_eq!(jump_at(src, def), None);

        let src = "see [^1] here\n\n[^1]: note\n";
        assert_eq!(jump_at(src, src.find('[').unwrap()), None);
        assert_eq!(jump_at(src, src.rfind('[').unwrap()), None);

        let src = "see <http://x.com> now";
        let open = src.find('<').unwrap();
        let close = src.find('>').unwrap();
        assert_eq!(jump_at(src, open), Some(close));
        assert_eq!(jump_at(src, close), Some(open));

        assert_eq!(jump_at("a < b > c", "a < b > c".find('<').unwrap()), None);
        assert_eq!(jump_at("plain", 1), None);
        let (doc, _) = parsed("see [a](u)");
        assert_eq!(
            bracket_target(&doc, Selection::caret(doc.len()), None),
            None
        );
    }

    #[test]
    fn destination_parens_and_empty_links_stay_with_their_own_pair() {
        // A `(` in the title used to take the destination's `)`.
        let src = "[a](u \"t(y\")";
        let open = src.find('(').unwrap();
        assert_eq!(jump_at(src, open), Some(src.rfind(')').unwrap()));
        assert_eq!(jump_at(src, src.find("t(").unwrap() + 1), None);

        // Balanced parentheses inside the URL stay put.
        let src = "[a](foo(bar))";
        let open = src.find('(').unwrap();
        let inner = src.find("(bar)").unwrap();
        assert_eq!(jump_at(src, open), Some(src.rfind(')').unwrap()));
        assert_eq!(jump_at(src, inner), None);
        assert_eq!(jump_at(src, src.rfind(')').unwrap()), Some(open));

        // A backslash-escaped parenthesis is not a partner.
        let src = "[a](foo\\(bar)";
        let open = src.find('(').unwrap();
        let escaped = src.find("\\(").unwrap() + 1;
        assert_eq!(jump_at(src, open), Some(src.rfind(')').unwrap()));
        assert_eq!(jump_at(src, escaped), None);
        assert_eq!(jump_at(src, src.rfind(')').unwrap()), Some(open));

        let src = "[a](u \"say \\) hi\")";
        let open = src.find('(').unwrap();
        let escaped = src.find("\\)").unwrap() + 1;
        let end = src.rfind(')').unwrap();
        assert_ne!(escaped, end);
        assert_eq!(jump_at(src, open), Some(end));
        assert_eq!(jump_at(src, escaped), None);
        assert_eq!(jump_at(src, end), Some(open));

        // Pointy destinations still jump on the parentheses around them.
        let src = "[a](<http://x.com>)";
        assert_eq!(
            jump_at(src, src.find('(').unwrap()),
            Some(src.rfind(')').unwrap())
        );
        assert_eq!(
            jump_at(src, src.find('<').unwrap()),
            Some(src.find('>').unwrap())
        );

        // An empty link is one span, and it jumps inside itself.
        let src = "[](u)";
        assert_eq!(jump_at(src, 0), Some(1));
        assert_eq!(jump_at(src, 1), Some(0));
        assert_eq!(jump_at(src, 2), Some(4));
        assert_eq!(jump_at(src, 4), Some(2));

        let src = "![](img.png)";
        assert_eq!(jump_at(src, 0), None);
        assert_eq!(jump_at(src, 1), Some(2));
        assert_eq!(
            jump_at(src, src.find('(').unwrap()),
            Some(src.find(')').unwrap())
        );

        // The empty link does not take the following link's brackets.
        let src = "[](u) and [b](v)";
        assert_eq!(jump_at(src, 0), Some(1));
        let second = src.rfind('[').unwrap();
        assert_eq!(jump_at(src, second), Some(src.rfind(']').unwrap()));

        // Nor the closer of an image whose alt text holds it.
        let src = "![a [](u) b](i.png)";
        assert_eq!(jump_at(src, 1), Some(src.rfind(']').unwrap()));
        let empty = src.find("[](u)").unwrap();
        assert_eq!(jump_at(src, empty), Some(empty + 1));
        assert_eq!(jump_at(src, empty + 2), Some(src.find(')').unwrap()));
        assert_eq!(
            jump_at(src, src.rfind('(').unwrap()),
            Some(src.rfind(')').unwrap())
        );

        let src = "![a ![](u) b](i.png)";
        assert_eq!(jump_at(src, 1), Some(src.rfind(']').unwrap()));
        let inner = src.find("![]").unwrap() + 1;
        assert_eq!(jump_at(src, inner), Some(inner + 1));

        // An unclosed `[` is not link markup, so the next link keeps its pair.
        let src = "text [unclosed then [b](y)";
        assert_eq!(jump_at(src, src.find('[').unwrap()), None);
        assert_eq!(
            jump_at(src, src.rfind('[').unwrap()),
            Some(src.rfind(']').unwrap())
        );
    }
}
