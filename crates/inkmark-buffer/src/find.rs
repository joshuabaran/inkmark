//! Find and replace over a rope, by UTF-8 byte offset.
//!
//! The matcher walks rope chunks. Callers keep the query and the
//! replacement; nothing here copies the document into one string.

use std::ops::Range;

use regex_cursor::engines::meta::Regex;
use regex_cursor::regex_automata::Anchored;
use regex_cursor::regex_automata::util::captures::Captures;
use regex_cursor::regex_automata::util::interpolate;
use regex_cursor::regex_automata::util::syntax;
use regex_cursor::{Input, RopeyCursor};
use ropey::Rope;

use crate::Document;
use crate::edit::{Edit, Selection};
use crate::history::EditKind;

/// How a query is interpreted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub regex: bool,
}

/// A compiled query. Plain text is a literal pattern, so a replacement
/// is inserted unchanged. A regex replacement uses the regex crate's
/// `$` syntax: `$1`, `${1}`, `$name`, `${name}` and `$$`. `$1a` is the
/// group named `1a`, not group 1 followed by `a`.
pub struct Finder {
    regex: Regex,
    literal: bool,
}

impl Finder {
    /// Compiles `pattern`. An empty pattern is refused so it cannot match
    /// every position in the file.
    pub fn compile(pattern: &str, options: SearchOptions) -> Result<Self, String> {
        if pattern.is_empty() {
            return Err("empty pattern".into());
        }
        let source = if options.regex {
            pattern.to_string()
        } else {
            escape_literal(pattern)
        };
        let syntax = syntax::Config::new()
            .multi_line(true)
            .case_insensitive(!options.case_sensitive);
        let regex = Regex::builder()
            .syntax(syntax)
            .build(&source)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            regex,
            literal: !options.regex,
        })
    }

    /// The first match that starts at or after `from`.
    ///
    /// `from == len` still searches, so an empty match at the end (`$`)
    /// is reachable. `from > len` is past that match and finds nothing.
    pub fn next(&self, rope: &Rope, from: usize) -> Option<Range<usize>> {
        let len = rope.len_bytes();
        if from > len {
            return None;
        }
        self.match_in(rope, from..len)
    }

    /// The last match that starts before `before`.
    pub fn prev(&self, rope: &Rope, before: usize) -> Option<Range<usize>> {
        let mut last = None;
        for range in self.iter(rope, 0) {
            if range.start >= before {
                break;
            }
            last = Some(range);
        }
        last
    }

    /// The last match in the rope, including an empty one at the end.
    pub fn last(&self, rope: &Rope) -> Option<Range<usize>> {
        self.prev(rope, usize::MAX)
    }

    /// `true` when `range` is exactly a match.
    pub fn covers(&self, rope: &Rope, range: &Range<usize>) -> bool {
        self.next(rope, range.start).as_ref() == Some(range)
    }

    /// Every match in one pass, stopping the stored list at `limit`.
    /// The count still covers the whole rope. `origin` picks `probed`:
    /// the first match at or after that offset.
    pub fn collect_matches(&self, rope: &Rope, limit: usize, origin: usize) -> MatchList {
        let mut matches = Vec::new();
        let mut count = 0;
        let mut probed = None;
        let mut truncated = false;
        for range in self.iter(rope, 0) {
            count += 1;
            if probed.is_none() && range.start >= origin {
                probed = Some((range.clone(), count));
            }
            if truncated {
                continue;
            }
            if matches.len() == limit {
                truncated = true;
                matches.clear();
            } else {
                matches.push(range);
            }
        }
        MatchList {
            matches,
            count,
            complete: !truncated,
            probed,
        }
    }

    /// How many matches, and the 1-based index of `current` when it is one.
    pub fn census(&self, rope: &Rope, current: Option<&Range<usize>>) -> (usize, Option<usize>) {
        let mut count = 0;
        let mut index = None;
        for range in self.iter(rope, 0) {
            count += 1;
            if current == Some(&range) {
                index = Some(count);
            }
        }
        (count, index)
    }

    /// The text that would replace this match.
    pub fn replacement(&self, rope: &Rope, range: &Range<usize>, template: &str) -> String {
        if self.literal {
            return template.to_string();
        }
        let caps = self.captures(rope, range);
        let mut out = String::new();
        interpolate::string(
            template,
            |index, dst| dst.push_str(&group_text(rope, &caps, range, index)),
            |name| {
                let pattern = caps.pattern()?;
                caps.group_info().to_index(pattern, name)
            },
            &mut out,
        );
        out
    }

    /// Replaces `range` and leaves the caret after the new text.
    pub fn replace_one(
        &self,
        doc: &mut Document,
        range: Range<usize>,
        template: &str,
        selection: Selection,
    ) -> Result<Selection, String> {
        let insert = self.replacement(doc.rope(), &range, template);
        let after = Selection::caret(range.start + insert.len());
        if doc.slice(range.clone()).as_ref() == insert {
            return Ok(Selection {
                anchor: range.start,
                head: range.end,
            });
        }
        doc.apply(
            vec![Edit::replace(range, insert)],
            selection,
            after,
            EditKind::Other,
        )
        .map_err(|e| e.to_string())?;
        Ok(after)
    }

    /// Replaces every match as one undo step. `None` when nothing changed.
    /// The caret is the end of the earliest replacement.
    pub fn replace_all(
        &self,
        doc: &mut Document,
        template: &str,
        selection: Selection,
    ) -> Result<Option<Selection>, String> {
        let ranges: Vec<Range<usize>> = self.iter(doc.rope(), 0).collect();
        let mut first_end = None;
        let mut edits = Vec::new();
        for range in ranges {
            let insert = self.replacement(doc.rope(), &range, template);
            if doc.slice(range.clone()).as_ref() == insert {
                continue;
            }
            if first_end.is_none() {
                first_end = Some(range.start + insert.len());
            }
            edits.push(Edit::replace(range, insert));
        }
        let Some(end) = first_end else {
            return Ok(None);
        };
        // Later matches first, so each range is still valid and undo
        // restores them in reverse.
        edits.reverse();
        let after = Selection::caret(end);
        doc.apply(edits, selection, after, EditKind::Other)
            .map_err(|e| e.to_string())?;
        Ok(Some(after))
    }

    fn match_in(&self, rope: &Rope, range: Range<usize>) -> Option<Range<usize>> {
        let found = self.regex.find(self.input(rope, range))?;
        Some(found.start()..found.end())
    }

    fn iter<'a>(&'a self, rope: &'a Rope, from: usize) -> impl Iterator<Item = Range<usize>> + 'a {
        let len = rope.len_bytes();
        let from = from.min(len);
        self.regex
            .find_iter(self.input(rope, from..len))
            .map(|m| m.start()..m.end())
    }

    fn captures(&self, rope: &Rope, range: &Range<usize>) -> Captures {
        let mut caps = self.regex.create_captures();
        let len = rope.len_bytes();
        let mut input = self.input(rope, range.start.min(len)..len);
        input.set_anchored(Anchored::Yes);
        self.regex.captures(input, &mut caps);
        caps
    }

    fn input<'a>(&self, rope: &'a Rope, range: Range<usize>) -> Input<RopeyCursor<'a>> {
        let mut input = Input::new(RopeyCursor::new(rope.slice(..)));
        input.set_range(range);
        input
    }
}

/// One pass over the rope's matches.
pub struct MatchList {
    /// Every match, or empty when there were more than the limit.
    pub matches: Vec<Range<usize>>,
    pub count: usize,
    /// `matches` holds every match.
    pub complete: bool,
    /// The first match at or after the probe offset, and its 1-based index.
    pub probed: Option<(Range<usize>, usize)>,
}

fn group_text(rope: &Rope, caps: &Captures, whole: &Range<usize>, index: usize) -> String {
    match caps.get_group(index) {
        Some(span) => rope.byte_slice(span.start..span.end).to_string(),
        None if index == 0 => rope.byte_slice(whole.clone()).to_string(),
        None => String::new(),
    }
}

fn escape_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(pattern: &str) -> Finder {
        Finder::compile(
            pattern,
            SearchOptions {
                case_sensitive: false,
                regex: false,
            },
        )
        .unwrap()
    }

    fn sensitive(pattern: &str) -> Finder {
        Finder::compile(
            pattern,
            SearchOptions {
                case_sensitive: true,
                regex: false,
            },
        )
        .unwrap()
    }

    fn regex(pattern: &str) -> Finder {
        Finder::compile(
            pattern,
            SearchOptions {
                case_sensitive: true,
                regex: true,
            },
        )
        .unwrap()
    }

    fn rope(text: &str) -> Rope {
        Rope::from_str(text)
    }

    #[test]
    fn plain_next_and_prev_use_byte_offsets() {
        let text = "one cat\ntwo cat\n";
        let rope = rope(text);
        let finder = plain("cat");
        assert_eq!(finder.next(&rope, 0), Some(4..7));
        assert_eq!(finder.next(&rope, 7), Some(12..15));
        assert_eq!(finder.next(&rope, 15), None);
        assert_eq!(finder.prev(&rope, 12), Some(4..7));
        assert_eq!(finder.prev(&rope, 4), None);
        assert_eq!(finder.last(&rope), Some(12..15));
        let (count, index) = finder.census(&rope, Some(&(12..15)));
        assert_eq!(count, 2);
        assert_eq!(index, Some(2));
    }

    #[test]
    fn case_fold_and_literal_metacharacters() {
        let rope = rope("Café. (a+b)\n");
        assert_eq!(plain("café").next(&rope, 0), Some(0..5));
        assert_eq!(sensitive("café").next(&rope, 0), None);
        assert_eq!(sensitive("Café").next(&rope, 0), Some(0..5));
        // `.` and `+` are the characters, not a pattern.
        assert_eq!(plain(".").next(&rope, 0), Some(5..6));
        assert_eq!(plain("(a+b)").next(&rope, 0), Some(7..12));
    }

    #[test]
    fn a_match_past_a_chunk_boundary_is_found() {
        let mut text = "a".repeat(20_000);
        text.push_str("needle");
        text.push_str(&"b".repeat(20_000));
        let rope = rope(&text);
        let at = 20_000;
        assert_eq!(plain("needle").next(&rope, 0), Some(at..at + 6));
    }

    #[test]
    fn regex_multiline_groups_and_invalid_pattern() {
        let rope = rope("a1\na2\n");
        let finder = regex(r"^a(\d)");
        assert_eq!(finder.next(&rope, 0), Some(0..2));
        assert_eq!(finder.next(&rope, 2), Some(3..5));
        assert_eq!(finder.replacement(&rope, &(0..2), "[$1]"), "[1]");
        assert_eq!(finder.replacement(&rope, &(0..2), "${1}"), "1");
        assert_eq!(finder.replacement(&rope, &(0..2), "${1}0"), "10");
        // `$1a` names the group `1a`. This pattern has no such group.
        assert_eq!(finder.replacement(&rope, &(0..2), "$1a"), "");
        assert_eq!(finder.replacement(&rope, &(0..2), "$$0"), "$0");
        let grouped = regex("(a)b");
        let ab = Rope::from_str("ab");
        assert_eq!(grouped.replacement(&ab, &(0..2), "${1}x"), "ax");
        assert!(
            Finder::compile(
                "(",
                SearchOptions {
                    case_sensitive: true,
                    regex: true,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn empty_pattern_is_refused() {
        assert!(
            Finder::compile(
                "",
                SearchOptions {
                    case_sensitive: false,
                    regex: false,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn plain_replacement_keeps_dollar_text() {
        let doc = Document::from_text("a");
        let finder = plain("a");
        assert_eq!(finder.replacement(doc.rope(), &(0..1), "$1"), "$1");
    }

    #[test]
    fn replace_all_is_one_undo_step() {
        let mut doc = Document::from_text("one cat\ntwo cat\n");
        let finder = plain("cat");
        let sel = Selection::caret(0);
        let after = finder.replace_all(&mut doc, "dog", sel).unwrap().unwrap();
        assert_eq!(doc.slice(0..doc.len()).as_ref(), "one dog\ntwo dog\n");
        assert_eq!(after, Selection::caret(7));
        assert_eq!(doc.undo(), Some(sel));
        assert_eq!(doc.slice(0..doc.len()).as_ref(), "one cat\ntwo cat\n");
        assert!(!doc.can_undo());
    }

    #[test]
    fn replace_one_then_the_next_match_is_still_there() {
        let mut doc = Document::from_text("cat cat");
        let finder = plain("cat");
        let after = finder
            .replace_one(&mut doc, 0..3, "dog", Selection::caret(0))
            .unwrap();
        assert_eq!(after, Selection::caret(3));
        assert_eq!(doc.slice(0..doc.len()).as_ref(), "dog cat");
        assert_eq!(finder.next(doc.rope(), after.head), Some(4..7));
        doc.undo();
        assert_eq!(doc.slice(0..doc.len()).as_ref(), "cat cat");
    }

    #[test]
    fn overlapping_matches_are_leftmost() {
        let rope = rope("aaaa");
        let finder = sensitive("aa");
        assert_eq!(finder.next(&rope, 0), Some(0..2));
        assert_eq!(finder.next(&rope, 2), Some(2..4));
        // The search starts at 1, so the match that begins there wins.
        assert_eq!(finder.next(&rope, 1), Some(1..3));
    }

    #[test]
    fn an_empty_match_at_the_end_is_reachable() {
        let rope = rope("ab");
        let finder = regex("$");
        assert_eq!(finder.next(&rope, 0), Some(2..2));
        assert_eq!(finder.next(&rope, 2), Some(2..2));
        assert_eq!(finder.next(&rope, 3), None);
        assert!(finder.covers(&rope, &(2..2)));
        let (count, index) = finder.census(&rope, Some(&(2..2)));
        assert_eq!((count, index), (1, Some(1)));
        assert_eq!(finder.last(&rope), Some(2..2));

        let empty = Rope::from_str("");
        assert_eq!(regex("$").next(&empty, 0), Some(0..0));
    }

    #[test]
    fn zero_length_matches_advance() {
        let rope = rope("ab");
        let finder = regex("a*");
        let mut at = 0;
        let mut seen = Vec::new();
        for _ in 0..6 {
            let Some(range) = finder.next(&rope, at) else {
                break;
            };
            seen.push(range.clone());
            at = if range.is_empty() { at + 1 } else { range.end };
        }
        assert!(seen.len() <= 4, "{seen:?}");
        assert!(seen.contains(&(0..1)));
    }
}
