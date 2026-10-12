//! The find bar. Its fields hold the query and the replacement; the
//! document stays in the rope.
//!
//! A plain query is counted on the UI thread. A regex query is counted
//! on a worker: the first match at the caret is selected when the walk
//! reaches it, and the count follows. The rope the worker sees is a
//! snapshot, so a newer query or a document edit drops that walk.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::Duration;

use eframe::egui::{self, Key, Modifiers, RichText, TextEdit};
use inkmark_buffer::find::{Finder, MatchList, Scan, SearchOptions};
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
    /// Set when a count is in flight and the index should be taken from
    /// this range once the walk finishes (an undo, or F3 during the walk).
    reindex: Option<Range<usize>>,
    generation: u64,
    scanning: bool,
    scan_epoch: u64,
    worker: Option<Worker>,
}

struct Worker {
    jobs: Sender<Job>,
    events: Receiver<Event>,
    shared: Arc<ScanShared>,
}

struct ScanShared {
    stop: AtomicBool,
    generation: AtomicU64,
}

struct Job {
    generation: u64,
    epoch: u64,
    scan: Scan,
    select_probe: bool,
    clears_index: bool,
}

enum Event {
    Probe {
        generation: u64,
        epoch: u64,
        range: Range<usize>,
        index: usize,
    },
    Done {
        generation: u64,
        epoch: u64,
        list: MatchList,
        clears_index: bool,
    },
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
        self.reindex = Some(selection.range());
        self.matches_ready = false;
        self.refresh(doc);
        if !self.scanning {
            self.apply_reindex(doc);
        }
    }

    /// The regex walk has not published its count yet.
    #[cfg(test)]
    pub(crate) fn scanning(&self) -> bool {
        self.scanning
    }

    /// Drops the match list when the document is replaced. The query,
    /// options, and replacement stay. The next time the bar is shown it
    /// searches the new document: a new document starts at epoch 0, the
    /// same epoch a search in an unedited file recorded, and `refresh`
    /// would otherwise keep the old ranges and slice them out of the new rope.
    pub fn reset_matches(&mut self) {
        self.clear_matches();
        self.force = true;
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
        // The query is known before the count is drawn. A regex walk that
        // started on an earlier frame publishes its first match here.
        let mut selection_out =
            if before != (self.query.clone(), self.case_sensitive, self.regex) || self.force {
                self.force = false;
                self.research(doc)
            } else {
                self.poll(doc)
            };
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
        if self.scanning {
            ui.ctx().request_repaint_after(Duration::from_millis(16));
        }

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
        self.poll(doc);
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
        self.poll(doc);
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
        self.reindex = None;
        let origin = self.origin.min(doc.len());
        if self.regex {
            // The walk counts every match. That stays off this thread.
            // Navigation still uses the compiled finder, one match at a time.
            self.finder = Some(finder.clone());
            self.clear_matches();
            self.start_scan(doc, origin, true, false, finder);
            return None;
        }
        // Drop a regex walk that this plain query replaces.
        self.clear_matches();
        let list = finder.collect_matches(doc.rope(), MATCH_LIST_LIMIT, origin);
        let probed = list.probed.clone();
        self.finder = Some(finder);
        self.install(doc, list);
        self.index = probed.as_ref().map(|(_, index)| *index);
        self.indexed = probed.as_ref().map(|(range, _)| range.clone());
        probed.map(|(range, _)| selection_of(range))
    }

    fn replace_one(&mut self, doc: &mut Document, selection: Selection) -> Option<Selection> {
        self.poll(doc);
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
        self.poll(doc);
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
        self.generation = self.generation.wrapping_add(1);
        if let Some(worker) = &self.worker {
            worker
                .shared
                .generation
                .store(self.generation, Ordering::Release);
        }
        self.scanning = false;
        self.forget_list();
    }

    fn forget_list(&mut self) {
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
        let Some(finder) = self.finder.clone() else {
            self.clear_matches();
            return;
        };
        if self.regex {
            if self.matches_ready && self.matches_epoch == doc.epoch() && !self.scanning {
                return;
            }
            if self.scanning && self.scan_epoch == doc.epoch() {
                return;
            }
            let clears_index = self.matches_ready;
            self.start_scan(doc, 0, false, clears_index, finder);
            self.forget_list();
            return;
        }
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
        // The in-flight walk publishes the index. Census would rescan the
        // whole rope on this thread.
        if self.scanning {
            return None;
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
        if self.scanning {
            self.reindex = found.clone();
            self.index = None;
            self.indexed = None;
            return found.map(selection_of);
        }
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
        } else if self.scanning && !self.matches_ready {
            if let Some(index) = self.index {
                format!("{index} of …")
            } else {
                "Searching".into()
            }
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

impl FindState {
    fn start_scan(
        &mut self,
        doc: &Document,
        origin: usize,
        select_probe: bool,
        clears_index: bool,
        finder: Finder,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.ensure_worker();
        let Some(worker) = &self.worker else {
            return;
        };
        worker
            .shared
            .generation
            .store(generation, Ordering::Release);
        self.scanning = true;
        self.scan_epoch = doc.epoch();
        let job = Job {
            generation,
            epoch: doc.epoch(),
            scan: Scan::new(finder, doc.rope(), origin, MATCH_LIST_LIMIT),
            select_probe,
            clears_index,
        };
        let _ = worker.jobs.send(job);
    }

    fn ensure_worker(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let (jobs_tx, jobs_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::channel();
        let shared = Arc::new(ScanShared {
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(0),
        });
        let worker_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("inkmark-find".into())
            .spawn(move || find_worker(jobs_rx, events_tx, worker_shared))
            .expect("spawn find thread");
        self.worker = Some(Worker {
            jobs: jobs_tx,
            events: events_rx,
            shared,
        });
    }

    /// Applies a finished regex walk. The selection is the probed match
    /// when this frame is the one that found it.
    fn poll(&mut self, doc: &Document) -> Option<Selection> {
        let Some(worker) = &self.worker else {
            return None;
        };
        let mut events = Vec::new();
        while let Ok(event) = worker.events.try_recv() {
            events.push(event);
        }
        let mut selection = None;
        for event in events {
            if let Some(found) = self.apply_event(doc, event) {
                selection = Some(found);
            }
        }
        selection
    }

    fn apply_event(&mut self, doc: &Document, event: Event) -> Option<Selection> {
        let (generation, epoch) = match &event {
            Event::Probe {
                generation, epoch, ..
            }
            | Event::Done {
                generation, epoch, ..
            } => (*generation, *epoch),
        };
        if generation != self.generation || epoch != doc.epoch() {
            return None;
        }
        match event {
            Event::Probe { range, index, .. } => {
                // A step during the walk owns the caret. Don't pull it back.
                if self.reindex.is_some() {
                    return None;
                }
                self.index = Some(index);
                self.indexed = Some(range.clone());
                Some(selection_of(range))
            }
            Event::Done {
                list, clears_index, ..
            } => {
                self.install(doc, list);
                self.scanning = false;
                if clears_index {
                    self.index = None;
                    self.indexed = None;
                }
                self.apply_reindex(doc);
                None
            }
        }
    }

    fn apply_reindex(&mut self, doc: &Document) {
        let Some(range) = self.reindex.take() else {
            return;
        };
        if self.matches_complete {
            self.index = self.index_of(doc, &range);
            self.indexed = self.index.is_some().then_some(range);
        }
    }

    #[cfg(test)]
    fn settle(&mut self, doc: &Document) {
        self.ensure(doc);
        let start = std::time::Instant::now();
        while self.scanning {
            self.poll(doc);
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "regex scan did not finish"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        self.poll(doc);
    }
}

fn find_worker(jobs: Receiver<Job>, events: Sender<Event>, shared: Arc<ScanShared>) {
    while let Ok(mut job) = jobs.recv() {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        loop {
            match jobs.try_recv() {
                Ok(newer) => job = newer,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let generation = job.generation;
        let epoch = job.epoch;
        let select_probe = job.select_probe;
        let clears_index = job.clears_index;
        let mut sent = false;
        let finished = job.scan.run(
            |range, index| {
                if !select_probe || sent {
                    return;
                }
                sent = true;
                if events
                    .send(Event::Probe {
                        generation,
                        epoch,
                        range,
                        index,
                    })
                    .is_err()
                {
                    // The bar is gone. `keep_going` ends the walk.
                    shared.stop.store(true, Ordering::Release);
                }
            },
            || {
                !shared.stop.load(Ordering::Acquire)
                    && shared.generation.load(Ordering::Acquire) == generation
            },
        );
        let still = !shared.stop.load(Ordering::Acquire)
            && shared.generation.load(Ordering::Acquire) == generation;
        if !still {
            continue;
        }
        let Some(list) = finished else {
            continue;
        };
        if events
            .send(Event::Done {
                generation,
                epoch,
                list,
                clears_index,
            })
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_matches_keeps_the_query_and_drops_the_old_ranges() {
        let old = Document::from_text("aaaa aaaa cat\n");
        let mut bar = FindState::default();
        bar.open(0, Some("cat".into()), false, false);
        let hit = bar.goto_next(&old, Selection::caret(0)).unwrap();
        assert_eq!(hit.range(), 10..13);
        bar.reset_matches();
        let new = Document::from_text("cat dog\n");
        let hit = bar.goto_next(&new, Selection::caret(0)).unwrap();
        assert_eq!(hit.range(), 0..3);
    }

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
        bar.settle(&doc);
        let end = bar.goto_next(&doc, Selection::caret(0)).unwrap();
        assert_eq!(end.range(), 2..2);
        assert_eq!(bar.match_index(), Some(1));
        assert_eq!(bar.match_count(), 1);
        let again = bar.goto_next(&doc, end).unwrap();
        assert_eq!(again.range(), 2..2);
        assert_eq!(bar.match_index(), Some(1));
    }

    #[test]
    fn a_regex_query_is_counted_off_this_thread() {
        let doc = Document::from_text(&"quickly ".repeat(40_000));
        let mut bar = FindState {
            regex: true,
            ..FindState::default()
        };
        bar.open(0, Some(r"\w+ly".into()), false, false);
        assert!(bar.research(&doc).is_none());
        assert!(bar.scanning());
        assert_eq!(bar.match_count(), 0);
        bar.settle(&doc);
        assert!(!bar.scanning());
        assert_eq!(bar.match_count(), 40_000);
        assert_eq!(bar.match_index(), Some(1));
    }

    #[test]
    fn a_plain_query_drops_a_regex_walk() {
        let mut text = "quickly ".repeat(20_000);
        text.push_str("xyzzy");
        let doc = Document::from_text(&text);
        let mut bar = FindState {
            regex: true,
            ..FindState::default()
        };
        bar.open(0, Some(r"\w+ly".into()), false, false);
        bar.research(&doc);
        assert!(bar.scanning());
        bar.regex = false;
        bar.query = "xyzzy".into();
        let found = bar.research(&doc).unwrap();
        assert!(!bar.scanning());
        assert_eq!(found.range(), text.len() - 5..text.len());
        assert_eq!(bar.match_count(), 1);
        assert_eq!(bar.match_index(), Some(1));
        std::thread::sleep(Duration::from_millis(50));
        assert!(bar.poll(&doc).is_none());
        assert_eq!(bar.match_count(), 1);
        assert_eq!(bar.match_index(), Some(1));
    }

    #[test]
    fn an_invalid_regex_is_reported_without_a_scan() {
        let doc = Document::from_text("a\n");
        let mut bar = FindState {
            regex: true,
            ..FindState::default()
        };
        bar.query = "(".into();
        assert!(bar.research(&doc).is_none());
        assert!(bar.error.is_some());
        assert!(!bar.scanning());
        assert!(bar.worker.is_none());
    }
}
