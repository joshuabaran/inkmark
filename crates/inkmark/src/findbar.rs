//! The find bar. Its fields hold the query and the replacement; the
//! document stays in the rope.

use std::ops::Range;

use eframe::egui::{self, Key, Modifiers, RichText, TextEdit};
use inkmark_buffer::find::{Finder, SearchOptions};
use inkmark_buffer::{Document, Selection};
use inkmark_view::theme;

const QUERY: &str = "find_query";
const REPLACE: &str = "find_replace";

/// A selection copied into the query has to stay a single short line.
const MAX_SEED: usize = 256;

/// Past this many matches the list is dropped. A pattern that hits on
/// every byte is counted, and each step searches again.
const MATCH_LIST_LIMIT: usize = 50_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Query,
    Replace,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Command {
    None,
    Next,
    Prev,
    ReplaceOne,
    ReplaceAll,
}

/// What the bar did this frame, for the panes to follow.
#[derive(Default)]
pub struct FindStep {
    pub selection: Option<Selection>,
    pub closed: bool,
}

/// The query for one document. Whether the bar is on screen lives on the
/// shell, so switching documents later can keep the bar up and show that
/// document's query.
#[derive(Default)]
pub struct FindState {
    query: String,
    replacement: String,
    case_sensitive: bool,
    regex: bool,
    error: Option<String>,
    /// Focus this field on the next draw.
    pending: Option<Field>,
    select_query: bool,
    /// Where incremental search starts. Next and previous do not move it.
    origin: usize,
    /// Re-run the search even when the query text is unchanged.
    force: bool,
    finder: Option<Finder>,
    matches: Vec<Range<usize>>,
    matches_epoch: u64,
    matches_complete: bool,
    matches_ready: bool,
    /// The match `index` refers to. Cleared when the document changes.
    indexed: Option<Range<usize>>,
    count: usize,
    index: Option<usize>,
}

impl FindState {
    #[cfg(test)]
    pub fn set_replacement(&mut self, text: impl Into<String>) {
        self.replacement = text.into();
    }

    #[cfg(test)]
    pub fn match_index(&self) -> Option<usize> {
        self.index
    }

    #[cfg(test)]
    pub fn match_count(&self) -> usize {
        self.count
    }

    /// The query or the replacement has the caret.
    pub fn field_focused(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| m.has_focus(egui::Id::new(QUERY)) || m.has_focus(egui::Id::new(REPLACE)))
    }

    /// Recounts after an undo or redo that happened outside the bar.
    pub fn sync_count(&mut self, doc: &Document, selection: Selection) {
        self.matches_ready = false;
        self.refresh(doc);
        let range = selection.range();
        self.index = self.index_of(doc, &range);
        self.indexed = self.index.is_some().then_some(range);
    }

    /// Arms a search. A short single-line selection becomes the query when
    /// the bar was closed. `replace` puts the caret in the replacement.
    /// `already_open` is the shell's bar, not this document's query.
    pub fn open(&mut self, origin: usize, seed: Option<String>, replace: bool, already_open: bool) {
        if !already_open {
            if let Some(seed) = seed {
                self.query = seed;
                self.select_query = true;
            }
        } else if !replace {
            self.select_query = true;
        }
        self.origin = origin;
        self.force = true;
        self.pending = Some(if replace {
            Field::Replace
        } else {
            Field::Query
        });
    }

    /// The bytes of `range` when they can seed the query.
    pub fn seed(text: &str) -> Option<String> {
        if text.is_empty() || text.len() > MAX_SEED || text.contains(['\n', '\r']) {
            None
        } else {
            Some(text.to_string())
        }
    }

    /// Draws the bar and applies whatever the user just asked for.
    /// Escape closes it unless a dialog is already using that key.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        doc: &mut Document,
        selection: Selection,
        modal: bool,
    ) -> FindStep {
        let query_id = egui::Id::new(QUERY);
        let replace_id = egui::Id::new(REPLACE);
        let query_focused = ui.memory(|m| m.has_focus(query_id));
        let replace_focused = ui.memory(|m| m.has_focus(replace_id));
        if !query_focused && !replace_focused {
            self.origin = selection.range().start.min(doc.len());
        }

        // Enter stays with a focused field. Escape closes the bar from a
        // button as well, and a dialog keeps Escape when `modal` is set.
        let (enter, shift_enter, escape) = ui.input_mut(|i| {
            let escape = if modal {
                false
            } else {
                i.consume_key(Modifiers::NONE, Key::Escape)
            };
            let (enter, shift_enter) = if query_focused || replace_focused {
                (
                    i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::SHIFT, Key::Enter),
                )
            } else {
                (false, false)
            };
            (enter, shift_enter, escape)
        });
        if escape {
            return FindStep {
                selection: None,
                closed: true,
            };
        }

        let command = if query_focused && enter {
            Command::Next
        } else if shift_enter {
            Command::Prev
        } else if replace_focused && enter {
            Command::ReplaceOne
        } else {
            Command::None
        };

        let before = (self.query.clone(), self.case_sensitive, self.regex);
        ui.horizontal(|ui| {
            ui.label("Find");
            let edit = TextEdit::singleline(&mut self.query)
                .id(query_id)
                .desired_width(280.0)
                .show(ui);
            if self.select_query {
                self.select_query = false;
                select_all(&edit, ui.ctx(), query_id, self.query.chars().count());
            }
            if ui
                .selectable_label(self.case_sensitive, "Match case")
                .clicked()
            {
                self.case_sensitive ^= true;
            }
            if ui.selectable_label(self.regex, "Regex").clicked() {
                self.regex ^= true;
            }
        });
        // The query is known before the count is drawn.
        let mut selection_out = None;
        if before != (self.query.clone(), self.case_sensitive, self.regex) || self.force {
            self.force = false;
            selection_out = self.research(doc);
        }
        let working = selection_out.unwrap_or(selection);
        if command == Command::Next {
            selection_out = self.goto_next(doc, working);
        } else if command == Command::Prev {
            selection_out = self.goto_prev(doc, working);
        }

        let mut button = Command::None;
        ui.horizontal(|ui| {
            ui.label(self.label());
            if ui.button("Previous").clicked() {
                button = Command::Prev;
            }
            if ui.button("Next").clicked() {
                button = Command::Next;
            }
            ui.label("Replace");
            TextEdit::singleline(&mut self.replacement)
                .id(replace_id)
                .desired_width(280.0)
                .show(ui);
            if ui.button("Replace one").clicked() {
                button = Command::ReplaceOne;
            }
            if ui.button("Replace all").clicked() {
                button = Command::ReplaceAll;
            }
        });
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(theme::current(ui.ctx()).error));
        }
        self.focus_pending(ui);

        // A click still moves when this frame's re-search already selected
        // a match. The key handled above is not applied twice.
        let working = selection_out.unwrap_or(selection);
        match button {
            Command::Next if command != Command::Next => {
                selection_out = self.goto_next(doc, working);
            }
            Command::Prev if command != Command::Prev => {
                selection_out = self.goto_prev(doc, working);
            }
            Command::ReplaceOne => selection_out = self.replace_one(doc, working),
            Command::ReplaceAll => selection_out = self.replace_all(doc, working),
            _ => {}
        }
        FindStep {
            selection: selection_out,
            closed: false,
        }
    }

    /// F3, including while the bar is closed.
    pub fn goto_next(&mut self, doc: &Document, selection: Selection) -> Option<Selection> {
        self.ensure(doc);
        self.refresh(doc);
        self.finder.as_ref()?;
        let range = selection.range();
        let on_match = self.indexed.as_ref() == Some(&range) || self.contains(doc, &range);
        let from = if on_match {
            step_past(doc, &range)
        } else {
            range.start
        };
        let mut found = self.locate_next(doc, from);
        let mut wrapped = false;
        if found.is_none() && from > 0 {
            found = self.locate_next(doc, 0);
            wrapped = found.is_some();
        }
        self.moved(doc, &range, found, true, wrapped)
    }

    /// Shift+F3.
    pub fn goto_prev(&mut self, doc: &Document, selection: Selection) -> Option<Selection> {
        self.ensure(doc);
        self.refresh(doc);
        self.finder.as_ref()?;
        let range = selection.range();
        let on_match = self.indexed.as_ref() == Some(&range) || self.contains(doc, &range);
        let before = if on_match { range.start } else { range.end };
        let mut found = self.locate_prev(doc, before);
        let mut wrapped = false;
        if found.is_none() && self.count > 0 {
            found = self.locate_last(doc);
            wrapped = found.is_some();
        }
        self.moved(doc, &range, found, false, wrapped)
    }

    fn research(&mut self, doc: &Document) -> Option<Selection> {
        if self.query.is_empty() {
            self.finder = None;
            self.error = None;
            self.clear_matches();
            return None;
        }
        let finder = match Finder::compile(&self.query, self.options()) {
            Ok(finder) => finder,
            Err(error) => {
                self.finder = None;
                self.error = Some(error);
                self.clear_matches();
                return None;
            }
        };
        self.error = None;
        let origin = self.origin.min(doc.len());
        let list = finder.collect_matches(doc.rope(), MATCH_LIST_LIMIT, origin);
        let probed = list.probed.clone();
        self.finder = Some(finder);
        self.install(doc, list);
        self.index = probed.as_ref().map(|(_, index)| *index);
        self.indexed = probed.as_ref().map(|(range, _)| range.clone());
        probed.map(|(range, _)| selection_of(range))
    }

    fn replace_one(&mut self, doc: &mut Document, selection: Selection) -> Option<Selection> {
        self.ensure(doc);
        let range = {
            let finder = self.finder.as_ref()?;
            let range = selection.range();
            if finder.covers(doc.rope(), &range) {
                range
            } else {
                finder.next(doc.rope(), range.start)?
            }
        };
        let template = self.replacement.clone();
        let replaced = {
            let finder = self.finder.as_ref()?;
            finder.replace_one(doc, range, &template, selection)
        };
        let after = match replaced {
            Ok(after) => after,
            Err(error) => {
                self.error = Some(error);
                return None;
            }
        };
        // The text just inserted is behind the caret, so the next match
        // is not the one that was replaced.
        self.goto_next(doc, after).or(Some(after))
    }

    fn replace_all(&mut self, doc: &mut Document, selection: Selection) -> Option<Selection> {
        self.ensure(doc);
        let template = self.replacement.clone();
        let replaced = {
            let finder = self.finder.as_ref()?;
            finder.replace_all(doc, &template, selection)
        };
        let after = match replaced {
            Ok(after) => after,
            Err(error) => {
                self.error = Some(error);
                return None;
            }
        };
        self.recount(doc);
        after
    }

    fn ensure(&mut self, doc: &Document) -> Option<&Finder> {
        if self.finder.is_none() && !self.query.is_empty() {
            self.research(doc);
        }
        self.finder.as_ref()
    }

    fn recount(&mut self, doc: &Document) {
        self.matches_ready = false;
        self.refresh(doc);
        self.index = None;
        self.indexed = None;
    }

    fn clear_matches(&mut self) {
        self.matches.clear();
        self.matches_complete = false;
        self.matches_ready = false;
        self.indexed = None;
        self.count = 0;
        self.index = None;
    }

    fn install(&mut self, doc: &Document, list: inkmark_buffer::find::MatchList) {
        self.count = list.count;
        self.matches = list.matches;
        self.matches_complete = list.complete;
        self.matches_epoch = doc.epoch();
        self.matches_ready = true;
    }

    /// Rebuilds the match list when the document has changed.
    fn refresh(&mut self, doc: &Document) {
        let Some(finder) = self.finder.as_ref() else {
            self.clear_matches();
            return;
        };
        if self.matches_ready && self.matches_epoch == doc.epoch() {
            return;
        }
        let had = self.matches_ready;
        let list = finder.collect_matches(doc.rope(), MATCH_LIST_LIMIT, 0);
        self.install(doc, list);
        if had {
            self.index = None;
            self.indexed = None;
        }
    }

    fn contains(&self, doc: &Document, range: &Range<usize>) -> bool {
        if self.matches_complete {
            return index_in(&self.matches, range).is_some();
        }
        self.finder
            .as_ref()
            .is_some_and(|finder| finder.covers(doc.rope(), range))
    }

    fn locate_next(&self, doc: &Document, from: usize) -> Option<Range<usize>> {
        if self.matches_complete {
            let i = self.matches.partition_point(|m| m.start < from);
            return self.matches.get(i).cloned();
        }
        self.finder.as_ref()?.next(doc.rope(), from)
    }

    fn locate_prev(&self, doc: &Document, before: usize) -> Option<Range<usize>> {
        if self.matches_complete {
            let i = self.matches.partition_point(|m| m.start < before);
            return (i > 0).then(|| self.matches[i - 1].clone());
        }
        self.finder.as_ref()?.prev(doc.rope(), before)
    }

    fn locate_last(&self, doc: &Document) -> Option<Range<usize>> {
        if self.matches_complete {
            return self.matches.last().cloned();
        }
        self.finder.as_ref()?.last(doc.rope())
    }

    fn index_of(&self, doc: &Document, range: &Range<usize>) -> Option<usize> {
        if self.matches_complete {
            return index_in(&self.matches, range);
        }
        self.finder.as_ref()?.census(doc.rope(), Some(range)).1
    }

    /// `on` is the match we stepped from, when the index names it.
    fn moved(
        &mut self,
        doc: &Document,
        on: &Range<usize>,
        found: Option<Range<usize>>,
        forward: bool,
        wrapped: bool,
    ) -> Option<Selection> {
        let known = self.indexed.as_ref() == Some(on) && self.index.is_some();
        if known && found.is_some() && !self.matches_complete {
            self.bump_index(forward, wrapped);
            if self.index.is_some() {
                self.indexed = found.clone();
                return found.map(selection_of);
            }
        }
        self.index = found.as_ref().and_then(|range| self.index_of(doc, range));
        self.indexed = self.index.is_some().then(|| found.clone()).flatten();
        found.map(selection_of)
    }

    fn bump_index(&mut self, forward: bool, wrapped: bool) {
        let Some(index) = self.index else { return };
        self.index = if wrapped {
            (self.count > 0).then_some(if forward { 1 } else { self.count })
        } else if forward {
            Some(index + 1).filter(|i| *i <= self.count)
        } else {
            index.checked_sub(1).filter(|i| *i > 0)
        };
    }

    fn options(&self) -> SearchOptions {
        SearchOptions {
            case_sensitive: self.case_sensitive,
            regex: self.regex,
        }
    }

    fn label(&self) -> String {
        if self.query.is_empty() || self.error.is_some() {
            String::new()
        } else if self.count == 0 {
            "No matches".into()
        } else if let Some(index) = self.index {
            format!("{index} of {}", self.count)
        } else {
            format!("{} matches", self.count)
        }
    }

    fn focus_pending(&mut self, ui: &mut egui::Ui) {
        let Some(field) = self.pending.take() else {
            return;
        };
        let id = egui::Id::new(match field {
            Field::Query => QUERY,
            Field::Replace => REPLACE,
        });
        ui.memory_mut(|m| m.request_focus(id));
    }
}

fn selection_of(range: std::ops::Range<usize>) -> Selection {
    Selection {
        anchor: range.start,
        head: range.end,
    }
}

fn step_past(doc: &Document, range: &Range<usize>) -> usize {
    if range.is_empty() {
        let next = doc.next_char_boundary(range.start);
        if next == range.start {
            // The empty match at the end. One past it, so the next search
            // does not land there again.
            doc.len().saturating_add(1)
        } else {
            next
        }
    } else {
        range.end
    }
}

fn index_in(matches: &[Range<usize>], range: &Range<usize>) -> Option<usize> {
    let i = matches.partition_point(|m| m.start < range.start);
    matches.get(i).filter(|m| *m == range).map(|_| i + 1)
}

fn select_all(
    edit: &egui::text_edit::TextEditOutput,
    ctx: &egui::Context,
    id: egui::Id,
    chars: usize,
) {
    use egui::text::{CCursor, CCursorRange};
    let mut state = edit.state.clone();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(0),
        CCursor::new(chars),
    )));
    state.store(ctx, id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f3_steps_and_wraps_without_losing_the_index() {
        let doc = Document::from_text("a\na\n");
        let mut bar = FindState::default();
        bar.open(0, Some("a".into()), false, false);
        let first = bar.goto_next(&doc, Selection::caret(0)).unwrap();
        assert_eq!(first.range(), 0..1);
        assert_eq!(bar.match_index(), Some(1));
        assert_eq!(bar.match_count(), 2);
        let second = bar.goto_next(&doc, first).unwrap();
        assert_eq!(second.range(), 2..3);
        assert_eq!(bar.match_index(), Some(2));
        let wrapped = bar.goto_next(&doc, second).unwrap();
        assert_eq!(wrapped.range(), 0..1);
        assert_eq!(bar.match_index(), Some(1));
        let back = bar.goto_prev(&doc, wrapped).unwrap();
        assert_eq!(back.range(), 2..3);
        assert_eq!(bar.match_index(), Some(2));
    }

    #[test]
    fn f3_reaches_the_empty_match_at_the_end_and_wraps() {
        let doc = Document::from_text("ab");
        let mut bar = FindState {
            regex: true,
            ..FindState::default()
        };
        bar.open(0, Some("$".into()), false, false);
        let end = bar.goto_next(&doc, Selection::caret(0)).unwrap();
        assert_eq!(end.range(), 2..2);
        assert_eq!(bar.match_index(), Some(1));
        assert_eq!(bar.match_count(), 1);
        let again = bar.goto_next(&doc, end).unwrap();
        assert_eq!(again.range(), 2..2);
        assert_eq!(bar.match_index(), Some(1));
    }
}
