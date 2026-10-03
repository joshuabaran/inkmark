//! The find bar. Its fields hold the query and the replacement; the
//! document stays in the rope.

use eframe::egui::{self, Key, Modifiers, RichText, TextEdit};
use inkmark_buffer::find::{Finder, SearchOptions};
use inkmark_buffer::{Document, Selection};
use inkmark_view::theme;

const QUERY: &str = "find_query";
const REPLACE: &str = "find_replace";

/// A selection copied into the query has to stay a single short line.
const MAX_SEED: usize = 256;

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

#[derive(Default)]
pub struct FindBar {
    open: bool,
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
    count: usize,
    index: Option<usize>,
}

impl FindBar {
    pub fn is_open(&self) -> bool {
        self.open
    }

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
        let Some(finder) = &self.finder else {
            self.count = 0;
            self.index = None;
            return;
        };
        let range = selection.range();
        let current = finder.covers(doc.rope(), &range).then_some(range);
        let (count, index) = finder.census(doc.rope(), current.as_ref());
        self.count = count;
        self.index = index;
    }

    /// Opens the bar. A short single-line selection becomes the query when
    /// the bar was closed. `replace` puts the caret in the replacement.
    pub fn open(&mut self, origin: usize, seed: Option<String>, replace: bool) {
        if !self.open {
            if let Some(seed) = seed {
                self.query = seed;
                self.select_query = true;
            }
        } else if !replace {
            self.select_query = true;
        }
        self.open = true;
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
            self.open = false;
            return FindStep {
                selection: None,
                closed: true,
            };
        }

        let mut command = if query_focused && enter {
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

        ui.horizontal(|ui| {
            ui.label(self.label());
            if ui.button("Previous").clicked() {
                command = Command::Prev;
            }
            if ui.button("Next").clicked() {
                command = Command::Next;
            }
            ui.label("Replace");
            TextEdit::singleline(&mut self.replacement)
                .id(replace_id)
                .desired_width(280.0)
                .show(ui);
            if ui.button("Replace one").clicked() {
                command = Command::ReplaceOne;
            }
            if ui.button("Replace all").clicked() {
                command = Command::ReplaceAll;
            }
        });
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(theme::current(ui.ctx()).error));
        }
        self.focus_pending(ui);

        // Buttons are reported as the row is drawn, so they apply after it.
        let working = selection_out.unwrap_or(selection);
        match command {
            Command::Next if selection_out.is_none() => {
                selection_out = self.goto_next(doc, working);
            }
            Command::Prev if selection_out.is_none() => {
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
        let found = {
            let finder = self.finder.as_ref()?;
            let range = selection.range();
            let from = if finder.covers(doc.rope(), &range) {
                step_past(doc, &range)
            } else {
                range.start
            };
            finder.next(doc.rope(), from).or_else(|| {
                if from > 0 {
                    finder.next(doc.rope(), 0)
                } else {
                    None
                }
            })
        };
        self.note(doc, found)
    }

    /// Shift+F3.
    pub fn goto_prev(&mut self, doc: &Document, selection: Selection) -> Option<Selection> {
        self.ensure(doc);
        let found = {
            let finder = self.finder.as_ref()?;
            let range = selection.range();
            let before = if finder.covers(doc.rope(), &range) {
                range.start
            } else {
                range.end
            };
            finder
                .prev(doc.rope(), before)
                .or_else(|| finder.last(doc.rope()))
        };
        self.note(doc, found)
    }

    fn research(&mut self, doc: &Document) -> Option<Selection> {
        if self.query.is_empty() {
            self.finder = None;
            self.error = None;
            self.count = 0;
            self.index = None;
            return None;
        }
        let finder = match Finder::compile(&self.query, self.options()) {
            Ok(finder) => finder,
            Err(error) => {
                self.finder = None;
                self.error = Some(error);
                self.count = 0;
                self.index = None;
                return None;
            }
        };
        self.error = None;
        let found = finder.next(doc.rope(), self.origin.min(doc.len()));
        let (count, index) = finder.census(doc.rope(), found.as_ref());
        self.count = count;
        self.index = index;
        self.finder = Some(finder);
        found.map(selection_of)
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
        if let Some(finder) = &self.finder {
            let (count, _) = finder.census(doc.rope(), None);
            self.count = count;
            self.index = None;
        }
    }

    fn note(&mut self, doc: &Document, found: Option<std::ops::Range<usize>>) -> Option<Selection> {
        if let Some(finder) = &self.finder {
            let (count, index) = finder.census(doc.rope(), found.as_ref());
            self.count = count;
            self.index = index;
        }
        found.map(selection_of)
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

fn step_past(doc: &Document, range: &std::ops::Range<usize>) -> usize {
    if range.is_empty() {
        let next = doc.next_char_boundary(range.start);
        if next == range.start { doc.len() } else { next }
    } else {
        range.end
    }
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
