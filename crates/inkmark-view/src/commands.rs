//! Editing commands as source patches: formatting toggles, links, heading
//! levels and the smart Enter / Backspace / Tab rules for lists and quotes.
//!
//! Everything works on source lines rather than the parse tree, so the
//! commands stay right even while the parse is a keystroke behind.

use std::ops::Range;

use egui::Key;
use inkmark_buffer::{Document, Edit, EditKind, Selection};

/// Edits to apply in order (each against the result of the previous), and
/// where the selection ends up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EditPlan {
    pub edits: Vec<Edit>,
    pub selection: Selection,
    pub kind: EditKind,
}

/// The container markup at the start of a source line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Prefix {
    /// Block quote markers, e.g. `"> > "`.
    quote: Range<usize>,
    /// Indentation between the quote and a list marker.
    indent: Range<usize>,
    /// The list marker itself, e.g. `-` or `12.`.
    marker: Option<Range<usize>>,
    /// Spaces after the marker.
    spacing: Range<usize>,
}

impl Prefix {
    /// Where the line's content starts (relative to the line).
    fn content_start(&self) -> usize {
        self.spacing.end
    }
}

/// Parses the quote and list markup at the start of `line`.
fn prefix(line: &str) -> Prefix {
    let b = line.as_bytes();
    let mut i = 0;
    // Block quote markers: up to 3 spaces, '>', one optional space; repeated.
    loop {
        let mut j = i;
        while j < b.len() && j - i < 3 && b[j] == b' ' {
            j += 1;
        }
        if j < b.len() && b[j] == b'>' {
            i = j + 1;
            if i < b.len() && b[i] == b' ' {
                i += 1;
            }
        } else {
            break;
        }
    }
    let quote = 0..i;
    let mut j = i;
    while j < b.len() && b[j] == b' ' {
        j += 1;
    }
    let indent = i..j;
    let marker_end = if j < b.len() && matches!(b[j], b'-' | b'*' | b'+') {
        Some(j + 1)
    } else {
        let digits = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
        let k = j + digits;
        (digits > 0 && digits <= 9 && k < b.len() && matches!(b[k], b'.' | b')')).then_some(k + 1)
    };
    // A marker needs a space after it (or the end of the line).
    let marker_end = marker_end.filter(|&k| k == b.len() || b[k] == b' ' || b[k] == b'\t');
    let Some(k) = marker_end else {
        return Prefix {
            quote,
            indent: i..i,
            marker: None,
            spacing: i..i,
        };
    };
    let mut s = k;
    while s < b.len() && s - k < 4 && b[s] == b' ' {
        s += 1;
    }
    Prefix {
        quote,
        indent,
        marker: Some(j..k),
        spacing: k..s,
    }
}

fn line_of(doc: &Document, offset: usize) -> (Range<usize>, String) {
    let range = doc.line_range(doc.byte_to_line(offset));
    let text = doc.slice(range.clone()).into_owned();
    (range, text)
}

/// The marker for the item after one with `marker` (bullets repeat,
/// numbers count up).
fn next_marker(marker: &str) -> String {
    let digits: String = marker.chars().take_while(char::is_ascii_digit).collect();
    match digits.parse::<u64>() {
        Ok(n) => format!("{}{}", n + 1, &marker[digits.len()..]),
        Err(_) => marker.to_owned(),
    }
}

fn caret_plan(edits: Vec<Edit>, caret: usize, kind: EditKind) -> EditPlan {
    EditPlan {
        edits,
        selection: Selection::caret(caret),
        kind,
    }
}

/// Context the caller knows from the parse.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EnterContext {
    /// The caret is inside a code block (fenced or indented) or HTML block.
    pub in_code: bool,
}

/// Enter: continue a list item or quote, leave it when the item is empty,
/// keep indentation in code, otherwise start a new paragraph.
pub(crate) fn smart_enter(doc: &Document, sel: Selection, ctx: EnterContext) -> EditPlan {
    let range = sel.range();
    let (line, text) = line_of(doc, range.start);
    let col = range.start - line.start;
    let insert = |s: String| {
        let caret = range.start + s.len();
        caret_plan(
            vec![Edit::replace(range.clone(), s)],
            caret,
            EditKind::Typing,
        )
    };
    if ctx.in_code {
        let indent: String = text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        return insert(format!("\n{indent}"));
    }
    let p = prefix(&text);
    let quote = &text[p.quote.clone()];
    let rest_empty = text[p.content_start()..].trim().is_empty();
    if let Some(marker) = &p.marker {
        if rest_empty && col >= p.content_start() {
            // Enter on an empty item ends the list: drop its marker and leave
            // a blank (quote) line, or the next text would continue the item.
            let keep = format!("{}\n{quote}", quote.trim_end());
            let caret = line.start + keep.len();
            return caret_plan(vec![Edit::replace(line, keep)], caret, EditKind::Other);
        }
        if col >= p.content_start() {
            // A marker at the end of the line has no spacing yet; add one.
            let spacing = match &text[p.spacing.clone()] {
                "" => " ",
                s => s,
            };
            let next = format!(
                "\n{quote}{}{}{spacing}",
                &text[p.indent.clone()],
                next_marker(&text[marker.clone()]),
            );
            return insert(next);
        }
    }
    if !p.quote.is_empty() && col >= p.quote.end {
        if rest_empty {
            // An empty quote line leaves the quote, with a blank line so the
            // next text doesn't continue the quoted paragraph.
            let caret = line.start + 1;
            return caret_plan(vec![Edit::replace(line, "\n")], caret, EditKind::Other);
        }
        // A new paragraph inside the quote needs a blank quote line between.
        return insert(format!("\n{}\n{quote}", quote.trim_end()));
    }
    insert("\n\n".to_owned())
}

/// Shift+Enter: a hard line break within the paragraph.
pub(crate) fn hard_break(doc: &Document, sel: Selection) -> EditPlan {
    let range = sel.range();
    let (_, text) = line_of(doc, range.start);
    let p = prefix(&text);
    // Continuation lines keep the quote and line up under the item text.
    let pad = " ".repeat(p.content_start() - p.quote.end);
    let s = format!("\\\n{}{pad}", &text[p.quote.clone()]);
    let caret = range.start + s.len();
    caret_plan(vec![Edit::replace(range, s)], caret, EditKind::Typing)
}

/// Backspace at the start of a list item, quote line or heading removes that
/// markup instead of a character. `None` when it doesn't apply.
pub(crate) fn smart_backspace(doc: &Document, sel: Selection) -> Option<EditPlan> {
    if !sel.is_empty() {
        return None;
    }
    let (line, text) = line_of(doc, sel.head);
    let col = sel.head - line.start;
    let p = prefix(&text);
    if let Some(marker) = &p.marker
        && col == p.content_start()
    {
        let remove = line.start + marker.start..line.start + p.spacing.end;
        let caret = remove.start;
        return Some(caret_plan(
            vec![Edit::delete(remove)],
            caret,
            EditKind::Other,
        ));
    }
    if p.marker.is_none() && !p.quote.is_empty() && col == p.quote.end {
        // Remove the innermost quote level.
        let q = &text[p.quote.clone()];
        let last = q.trim_end().rfind('>').expect("quote has a marker");
        let remove = line.start + last..line.start + p.quote.end;
        let caret = remove.start;
        return Some(caret_plan(
            vec![Edit::delete(remove)],
            caret,
            EditKind::Other,
        ));
    }
    let hashes = heading_marker(&text[p.content_start()..]);
    if hashes > 0 && col == p.content_start() + hashes {
        let start = line.start + p.content_start();
        let remove = start..start + hashes;
        return Some(caret_plan(
            vec![Edit::delete(remove)],
            start,
            EditKind::Other,
        ));
    }
    None
}

/// Length of an ATX heading marker (`## `) at the start of `s`, or 0.
fn heading_marker(s: &str) -> usize {
    let hashes = s.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return 0;
    }
    let spaces = s[hashes..].bytes().take_while(|&b| b == b' ').count();
    if spaces == 0 && hashes < s.len() {
        return 0;
    }
    hashes + spaces
}

/// Tab / Shift+Tab on list items: indent or outdent every selected item line
/// by its marker width. `None` if no selected line is a list item.
pub(crate) fn indent_list(doc: &Document, sel: Selection, outdent: bool) -> Option<EditPlan> {
    let range = sel.range();
    let first = doc.byte_to_line(range.start);
    let last = doc.byte_to_line(range.end);
    let mut edits = Vec::new();
    // How much an edit at `at` moves `offset`.
    let shift_before =
        |offset: usize, delta: isize, at: usize| if offset >= at { delta } else { 0 };
    let mut delta_start = 0isize;
    let mut delta_end = 0isize;
    // Later lines first so earlier offsets stay valid.
    for l in (first..=last).rev() {
        let line = doc.line_range(l);
        let text = doc.slice(line.clone()).into_owned();
        let p = prefix(&text);
        let Some(marker) = &p.marker else { continue };
        let width = (p.spacing.end - marker.start) as isize;
        let at = line.start + p.indent.start;
        let delta = if outdent {
            let remove = (p.indent.len() as isize).min(width);
            if remove == 0 {
                continue;
            }
            edits.push(Edit::delete(at..at + remove as usize));
            -remove
        } else {
            edits.push(Edit::insert(at, " ".repeat(width as usize)));
            width
        };
        delta_start += shift_before(range.start, delta, at);
        delta_end += shift_before(range.end, delta, at);
    }
    if edits.is_empty() {
        return None;
    }
    let map = |o: usize, d: isize| o.saturating_add_signed(d);
    let anchor_is_start = sel.anchor <= sel.head;
    let (start, end) = (map(range.start, delta_start), map(range.end, delta_end));
    Some(EditPlan {
        edits,
        selection: if anchor_is_start {
            Selection {
                anchor: start,
                head: end,
            }
        } else {
            Selection {
                anchor: end,
                head: start,
            }
        },
        kind: EditKind::Other,
    })
}

/// Wraps the selection in `marker` (e.g. `**`), or unwraps it if it is
/// already wrapped. `alternates` are other spellings of the same marker
/// (`_` for `*`) that also unwrap.
pub(crate) fn toggle_wrap(
    doc: &Document,
    sel: Selection,
    marker: &str,
    alternates: &[&str],
) -> EditPlan {
    let r = sel.range();
    let m = marker.len();
    for candidate in std::iter::once(marker).chain(alternates.iter().copied()) {
        let n = candidate.len();
        if r.start >= n
            && is_at(doc, r.start - n, candidate)
            && is_at(doc, r.end, candidate)
            // `*` must not be half of a `**`.
            && !(r.start > n && is_at(doc, r.start - n - 1, &candidate[..1]))
        {
            return EditPlan {
                edits: vec![
                    Edit::delete(r.end..r.end + n),
                    Edit::delete(r.start - n..r.start),
                ],
                selection: Selection {
                    anchor: r.start - n,
                    head: r.end - n,
                },
                kind: EditKind::Other,
            };
        }
    }
    if r.is_empty() {
        return caret_plan(
            vec![Edit::insert(r.start, format!("{marker}{marker}"))],
            r.start + m,
            EditKind::Other,
        );
    }
    EditPlan {
        edits: vec![Edit::insert(r.end, marker), Edit::insert(r.start, marker)],
        selection: Selection {
            anchor: r.start + m,
            head: r.end + m,
        },
        kind: EditKind::Other,
    }
}

/// Whether `doc` has `s` at byte `at` (false if that's mid-character or
/// past the end).
fn is_at(doc: &Document, at: usize, s: &str) -> bool {
    let end = at + s.len();
    end <= doc.len()
        && doc.is_char_boundary(at)
        && doc.is_char_boundary(end)
        && doc.slice(at..end) == s
}

/// Ctrl+K: `[selection](|)` with the caret in the parentheses, or `[|]()`.
pub(crate) fn insert_link(_doc: &Document, sel: Selection) -> EditPlan {
    let r = sel.range();
    if r.is_empty() {
        return caret_plan(
            vec![Edit::insert(r.start, "[]()")],
            r.start + 1,
            EditKind::Other,
        );
    }
    caret_plan(
        vec![Edit::insert(r.end, "]()"), Edit::insert(r.start, "[")],
        r.end + 3,
        EditKind::Other,
    )
}

/// Sets the caret line's heading level (0 = paragraph). Setting the level it
/// already has turns it back into a paragraph.
pub(crate) fn set_heading(doc: &Document, sel: Selection, level: u8) -> EditPlan {
    let (line, text) = line_of(doc, sel.head);
    let p = prefix(&text);
    let start = line.start + p.content_start();
    let current = heading_marker(&text[p.content_start()..]);
    let current_level = text[p.content_start()..]
        .bytes()
        .take_while(|&b| b == b'#')
        .count();
    let level = if current > 0 && current_level == usize::from(level) {
        0
    } else {
        level
    };
    let new = if level == 0 {
        String::new()
    } else {
        format!("{} ", "#".repeat(usize::from(level)))
    };
    let new_len = new.len();
    let delta = new_len as isize - current as isize;
    let shift = |o: usize| {
        if o >= start + current {
            o.saturating_add_signed(delta)
        } else {
            o.min(start + new_len)
        }
    };
    EditPlan {
        edits: vec![Edit::replace(start..start + current, new)],
        selection: Selection {
            anchor: shift(sel.anchor),
            head: shift(sel.head),
        },
        kind: EditKind::Other,
    }
}

/// Ctrl+Alt+0..6 set the heading level (0 = paragraph).
pub(crate) fn heading_level(key: Key) -> Option<u8> {
    Some(match key {
        Key::Num0 => 0,
        Key::Num1 => 1,
        Key::Num2 => 2,
        Key::Num3 => 3,
        Key::Num4 => 4,
        Key::Num5 => 5,
        Key::Num6 => 6,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Applies `plan` to `text` and renders the selection as `|` (caret) or
    /// `[...]` (selection).
    fn apply(
        text: &str,
        sel: Selection,
        plan: impl FnOnce(&Document, Selection) -> EditPlan,
    ) -> String {
        let mut doc = Document::from_text(text);
        let p = plan(&doc, sel);
        doc.apply(p.edits, sel, p.selection, p.kind).unwrap();
        let s = doc.slice(0..doc.len()).into_owned();
        let r = p.selection.range();
        if r.is_empty() {
            format!("{}|{}", &s[..r.start], &s[r.start..])
        } else {
            format!("{}[{}]{}", &s[..r.start], &s[r.clone()], &s[r.end..])
        }
    }

    /// `|` marks the caret, `[...]` a selection, in the input.
    fn at(text: &str) -> (String, Selection) {
        if let Some(i) = text.find('|') {
            return (text.replacen('|', "", 1), Selection::caret(i));
        }
        let a = text.find('[').unwrap();
        let b = text.find(']').unwrap() - 1;
        (
            text.replacen('[', "", 1).replacen(']', "", 1),
            Selection { anchor: a, head: b },
        )
    }

    fn enter(input: &str) -> String {
        let (text, sel) = at(input);
        apply(&text, sel, |d, s| {
            smart_enter(d, s, EnterContext::default())
        })
    }

    #[test]
    fn enter_continues_lists_and_quotes() {
        assert_eq!(enter("- one|"), "- one\n- |");
        assert_eq!(enter("  * one|"), "  * one\n  * |");
        assert_eq!(enter("9. nine|"), "9. nine\n10. |");
        assert_eq!(enter("3) x|"), "3) x\n4) |");
        assert_eq!(enter("- split| here"), "- split\n- | here");
        assert_eq!(enter("> - in quote|"), "> - in quote\n> - |");
        assert_eq!(enter("> quoted|"), "> quoted\n>\n> |");
        assert_eq!(enter("plain|"), "plain\n\n|");
    }

    #[test]
    fn enter_on_empty_item_or_quote_line_leaves_it() {
        assert_eq!(enter("- one\n- |"), "- one\n\n|");
        assert_eq!(enter("> - one\n> - |"), "> - one\n>\n> |");
        assert_eq!(enter("> one\n> |"), "> one\n\n|");
    }

    #[test]
    fn enter_in_code_keeps_indentation() {
        let (text, sel) = at("```\n    let x = 1;|\n```");
        let out = apply(&text, sel, |d, s| {
            smart_enter(d, s, EnterContext { in_code: true })
        });
        assert_eq!(out, "```\n    let x = 1;\n    |\n```");
    }

    #[test]
    fn shift_enter_is_a_hard_break() {
        let (text, sel) = at("- one| two");
        assert_eq!(apply(&text, sel, hard_break), "- one\\\n  | two");
    }

    #[test]
    fn backspace_at_content_start_removes_markup() {
        let bs = |input: &str| {
            let (text, sel) = at(input);
            let mut doc = Document::from_text(&text);
            let p = smart_backspace(&doc, sel)?;
            doc.apply(p.edits, sel, p.selection, p.kind).unwrap();
            let s = doc.slice(0..doc.len()).into_owned();
            let c = p.selection.head;
            Some(format!("{}|{}", &s[..c], &s[c..]))
        };
        assert_eq!(bs("- |item").as_deref(), Some("|item"));
        assert_eq!(bs("> - |item").as_deref(), Some("> |item"));
        assert_eq!(bs("> > |quoted").as_deref(), Some("> |quoted"));
        assert_eq!(bs("## |Title").as_deref(), Some("|Title"));
        assert_eq!(bs("- i|tem"), None);
        assert_eq!(bs("plain |text"), None);
    }

    #[test]
    fn tab_indents_and_outdents_items() {
        let tab = |input: &str, outdent: bool| {
            let (text, sel) = at(input);
            let mut doc = Document::from_text(&text);
            let p = indent_list(&doc, sel, outdent)?;
            doc.apply(p.edits, sel, p.selection, p.kind).unwrap();
            let s = doc.slice(0..doc.len()).into_owned();
            let c = p.selection.head;
            Some(format!("{}|{}", &s[..c], &s[c..]))
        };
        assert_eq!(tab("- a\n- b|", false).as_deref(), Some("- a\n  - b|"));
        assert_eq!(tab("1. a\n1. b|", false).as_deref(), Some("1. a\n   1. b|"));
        assert_eq!(tab("- a\n  - b|", true).as_deref(), Some("- a\n- b|"));
        assert_eq!(
            tab("> - a\n> - b|", false).as_deref(),
            Some("> - a\n>   - b|")
        );
        assert_eq!(tab("plain|", false), None);
    }

    #[test]
    fn wrap_and_unwrap() {
        let (text, sel) = at("some [bold] text");
        assert_eq!(
            apply(&text, sel, |d, s| toggle_wrap(d, s, "**", &[])),
            "some **[bold]** text"
        );
        let (text, sel) = at("some **[bold]** text");
        assert_eq!(
            apply(&text, sel, |d, s| toggle_wrap(d, s, "**", &[])),
            "some [bold] text"
        );
        // Italic unwraps either spelling, but not half of a strong marker.
        let (text, sel) = at("an _[it]_ word");
        assert_eq!(
            apply(&text, sel, |d, s| toggle_wrap(d, s, "*", &["_"])),
            "an [it] word"
        );
        let (text, sel) = at("**[b]**");
        assert_eq!(
            apply(&text, sel, |d, s| toggle_wrap(d, s, "*", &["_"])),
            "***[b]***"
        );
        let (text, sel) = at("x|y");
        assert_eq!(
            apply(&text, sel, |d, s| toggle_wrap(d, s, "`", &[])),
            "x`|`y"
        );
        // Multi-byte neighbours don't trip the marker check.
        let (text, sel) = at("é[x]é");
        assert_eq!(
            apply(&text, sel, |d, s| toggle_wrap(d, s, "**", &[])),
            "é**[x]**é"
        );
    }

    #[test]
    fn links_and_headings() {
        let (text, sel) = at("see [docs] now");
        assert_eq!(apply(&text, sel, insert_link), "see [docs](|) now");
        let (text, sel) = at("x|");
        assert_eq!(apply(&text, sel, insert_link), "x[|]()");
        let (text, sel) = at("Ti|tle");
        assert_eq!(apply(&text, sel, |d, s| set_heading(d, s, 2)), "## Ti|tle");
        let (text, sel) = at("## Ti|tle");
        assert_eq!(apply(&text, sel, |d, s| set_heading(d, s, 3)), "### Ti|tle");
        assert_eq!(apply(&text, sel, |d, s| set_heading(d, s, 2)), "Ti|tle");
        let (text, sel) = at("> - It|em");
        assert_eq!(
            apply(&text, sel, |d, s| set_heading(d, s, 1)),
            "> - # It|em"
        );
    }
}
