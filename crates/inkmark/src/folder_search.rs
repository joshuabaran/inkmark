//! Search across the open folder. File names are matched first. File
//! contents are read on a worker thread, the same way a full parse is:
//! the UI only sends the query and draws whatever has come back.

use std::io::ErrorKind;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, FontId, Key, Modifiers, RichText, ScrollArea, Sense, TextEdit};
use inkmark_buffer::Document;
use inkmark_buffer::find::{Finder, SearchOptions};
use inkmark_files::{NoteFile, walk_notes};
use inkmark_view::theme;

const QUERY: &str = "folder_query";

/// Same quiet gap as a full parse before the worker starts.
const DEBOUNCE: Duration = Duration::from_millis(24);

/// The find bar's cap. Past this, further matches are not stored.
const MATCH_CAP: usize = 50_000;

/// A note larger than this is still a file-name hit, and its text is not read.
const MAX_BYTES: u64 = 32 * 1024 * 1024;

const ROW_H: f32 = 22.0;

/// What the user asked to open from a result row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SearchOpen {
    File(PathBuf),
    Match { path: PathBuf, range: Range<usize> },
}

pub(crate) struct SearchFrame {
    pub body_top: f32,
    pub open: Option<SearchOpen>,
    pub showing_results: bool,
    pub closed: bool,
}

struct NameHit {
    path: PathBuf,
    relative: String,
}

struct ContentHit {
    path: PathBuf,
    relative: String,
    range: Range<usize>,
    preview: String,
}

struct ContentList {
    hits: Vec<ContentHit>,
    complete: bool,
}

struct Job {
    generation: u64,
    root: PathBuf,
    query: String,
    case_sensitive: bool,
    regex: bool,
    show_all: bool,
}

enum Report {
    Names {
        generation: u64,
        hits: Vec<NameHit>,
    },
    Content {
        generation: u64,
        hits: Vec<ContentHit>,
        complete: bool,
    },
    Failed {
        generation: u64,
        message: String,
    },
}

enum Row {
    Header(String),
    Status(String),
    Hit { index: usize, label: String },
}

pub(crate) struct FolderSearch {
    open: bool,
    query: String,
    case_sensitive: bool,
    regex: bool,
    error: Option<String>,
    pending_focus: bool,
    select_query: bool,
    root: PathBuf,
    show_all: bool,
    names: Vec<NameHit>,
    contents: Vec<ContentHit>,
    names_done: bool,
    content_done: bool,
    content_complete: bool,
    scanning: bool,
    selected: usize,
    scroll_to: Option<usize>,
    dirty: bool,
    last_edit: Instant,
    generation: u64,
    cancel: Arc<AtomicU64>,
    jobs: Sender<Job>,
    reports: Receiver<Report>,
}

impl FolderSearch {
    pub(crate) fn new(root: PathBuf, on_result: impl Fn() + Send + 'static) -> Self {
        let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
        let (reports_tx, reports) = mpsc::channel();
        let cancel = Arc::new(AtomicU64::new(0));
        let worker_cancel = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("inkmark-search".into())
            .spawn(move || worker(jobs_rx, reports_tx, worker_cancel, on_result))
            .expect("spawn search thread");
        Self {
            open: false,
            query: String::new(),
            case_sensitive: false,
            regex: false,
            error: None,
            pending_focus: false,
            select_query: false,
            root,
            show_all: false,
            names: Vec::new(),
            contents: Vec::new(),
            names_done: false,
            content_done: false,
            content_complete: true,
            scanning: false,
            selected: 0,
            scroll_to: None,
            dirty: false,
            last_edit: Instant::now(),
            generation: 0,
            cancel,
            jobs: jobs_tx,
            reports,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Ctrl+Shift+F. A query already typed is searched again if the last
    /// pass did not finish.
    pub(crate) fn open(&mut self) {
        self.open = true;
        self.pending_focus = true;
        self.select_query = !self.query.is_empty();
        if !self.query.is_empty() && !self.content_done {
            self.dirty = true;
            self.last_edit = Instant::now()
                .checked_sub(DEBOUNCE)
                .unwrap_or(self.last_edit);
        }
    }

    pub(crate) fn close(&mut self) {
        self.open = false;
        self.stop();
    }

    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The query is empty, the pattern was rejected, or both passes have landed.
    #[cfg(test)]
    pub(crate) fn settled(&self) -> bool {
        self.query.is_empty() || self.error.is_some() || (self.names_done && self.content_done)
    }

    /// The field, then the list. `body_top` is where the sidebar's rows go.
    pub(crate) fn show_bar(
        &mut self,
        ui: &mut egui::Ui,
        root: &Path,
        show_all: bool,
        modal: bool,
    ) -> SearchFrame {
        if self.root != root || self.show_all != show_all {
            self.root = root.to_path_buf();
            self.show_all = show_all;
            if !self.query.is_empty() {
                self.dirty = true;
                self.last_edit = Instant::now()
                    .checked_sub(DEBOUNCE)
                    .unwrap_or(self.last_edit);
            }
        }

        let query_id = egui::Id::new(QUERY);
        let query_focused = ui.memory(|m| m.has_focus(query_id));
        let showing = self.showing_results();
        let (escape, enter, up, down) = ui.input_mut(|input| {
            let escape = if modal {
                false
            } else {
                input.consume_key(Modifiers::NONE, Key::Escape)
            };
            let steer = query_focused && showing;
            (
                escape,
                steer && input.consume_key(Modifiers::NONE, Key::Enter),
                steer && input.consume_key(Modifiers::NONE, Key::ArrowUp),
                steer && input.consume_key(Modifiers::NONE, Key::ArrowDown),
            )
        });
        if escape {
            return SearchFrame {
                body_top: ui.cursor().min.y,
                open: None,
                showing_results: false,
                closed: true,
            };
        }

        let before = (self.query.clone(), self.case_sensitive, self.regex);
        ui.add_space(4.0);
        let edit = ui
            .horizontal(|ui| {
                TextEdit::singleline(&mut self.query)
                    .id(query_id)
                    .desired_width(ui.available_width())
                    .hint_text("Search this folder")
                    .show(ui)
            })
            .inner;
        if self.select_query {
            self.select_query = false;
            select_all(&edit, ui.ctx(), query_id, self.query.chars().count());
        }
        if self.pending_focus {
            self.pending_focus = false;
            ui.memory_mut(|m| m.request_focus(query_id));
        }
        ui.horizontal(|ui| {
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
        ui.separator();
        if before != (self.query.clone(), self.case_sensitive, self.regex) {
            self.dirty = true;
            self.last_edit = Instant::now();
            self.selected = 0;
        }
        self.pump(ui.ctx());

        let mut open = None;
        if up && self.hit_count() > 0 {
            self.selected = self.selected.saturating_sub(1);
            self.scroll_to = self.row_of(self.selected);
        }
        if down && self.hit_count() > 0 {
            let last = self.hit_count() - 1;
            if self.selected < last {
                self.selected += 1;
            }
            self.scroll_to = self.row_of(self.selected);
        }
        if enter {
            open = self.open_hit(self.selected);
        }
        SearchFrame {
            body_top: ui.cursor().min.y,
            open,
            showing_results: self.showing_results(),
            closed: false,
        }
    }

    pub(crate) fn show_results(&mut self, ui: &mut egui::Ui) -> Option<SearchOpen> {
        let colors = theme::current(ui.ctx());
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(colors.error));
            return None;
        }
        let rows = self.rows();
        let scroll_to = self.scroll_to.take();
        let mut area = ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt("folder_search");
        if let Some(index) = scroll_to {
            area = area.vertical_scroll_offset(index as f32 * ROW_H);
        }
        let mut clicked = None;
        area.show_rows(ui, ROW_H, rows.len(), |ui, range| {
            for index in range {
                let row = &rows[index];
                let width = ui.available_width();
                if !width.is_finite() {
                    continue;
                }
                let hit = matches!(row, Row::Hit { .. });
                let sense = if hit { Sense::click() } else { Sense::hover() };
                let (rect, response) = ui.allocate_exact_size(egui::vec2(width, ROW_H), sense);
                let label = match row {
                    Row::Header(text) | Row::Status(text) => text.clone(),
                    Row::Hit {
                        index: hit_index,
                        label,
                    } => {
                        if *hit_index == self.selected {
                            ui.painter().rect_filled(rect, 0.0, colors.selection);
                        }
                        label.clone()
                    }
                };
                let color = match row {
                    Row::Header(_) | Row::Status(_) => colors.markup,
                    Row::Hit { .. } => colors.text,
                };
                let font = FontId::proportional(13.0);
                let galley = fit_label(ui, label, font, color, (rect.width() - 16.0).max(0.0));
                let pos = egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y * 0.5);
                ui.painter().galley(pos, galley, color);
                if response.clicked()
                    && let Row::Hit { index, .. } = row
                {
                    clicked = Some(*index);
                }
            }
        });
        clicked.and_then(|index| {
            self.selected = index;
            self.open_hit(index)
        })
    }

    fn showing_results(&self) -> bool {
        !self.query.is_empty()
    }

    fn pump(&mut self, ctx: &egui::Context) {
        self.recv();
        if self.dirty {
            self.dirty = false;
            if self.query.is_empty() {
                self.stop();
                self.clear_hits();
                self.error = None;
            } else if let Err(error) = Finder::compile(&self.query, self.options()) {
                self.stop();
                self.clear_hits();
                self.error = Some(error);
            } else {
                self.error = None;
                let quiet = Instant::now().saturating_duration_since(self.last_edit);
                if quiet < DEBOUNCE {
                    self.dirty = true;
                    ctx.request_repaint_after(DEBOUNCE - quiet);
                } else {
                    self.send();
                }
            }
        }
        if self.scanning {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn recv(&mut self) {
        while let Ok(report) = self.reports.try_recv() {
            match report {
                Report::Names { generation, hits } if generation == self.generation => {
                    self.names = hits;
                    self.names_done = true;
                    self.clamp_selection();
                }
                Report::Content {
                    generation,
                    hits,
                    complete,
                } if generation == self.generation => {
                    self.contents = hits;
                    self.content_done = true;
                    self.content_complete = complete;
                    self.scanning = false;
                    self.clamp_selection();
                }
                Report::Failed {
                    generation,
                    message,
                } if generation == self.generation => {
                    self.clear_hits();
                    self.error = Some(message);
                    self.names_done = true;
                    self.content_done = true;
                    self.scanning = false;
                }
                _ => {}
            }
        }
    }

    fn send(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cancel.store(self.generation, Ordering::Release);
        self.clear_hits();
        self.names_done = false;
        self.content_done = false;
        self.content_complete = true;
        self.scanning = true;
        let _ = self.jobs.send(Job {
            generation: self.generation,
            root: self.root.clone(),
            query: self.query.clone(),
            case_sensitive: self.case_sensitive,
            regex: self.regex,
            show_all: self.show_all,
        });
    }

    fn stop(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cancel.store(self.generation, Ordering::Release);
        self.scanning = false;
    }

    fn clear_hits(&mut self) {
        self.names.clear();
        self.contents.clear();
        self.names_done = false;
        self.content_done = false;
        self.content_complete = true;
        self.selected = 0;
    }

    fn options(&self) -> SearchOptions {
        SearchOptions {
            case_sensitive: self.case_sensitive,
            regex: self.regex,
        }
    }

    fn hit_count(&self) -> usize {
        self.names.len() + self.contents.len()
    }

    fn clamp_selection(&mut self) {
        let count = self.hit_count();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    fn open_hit(&self, index: usize) -> Option<SearchOpen> {
        if let Some(hit) = self.names.get(index) {
            return Some(SearchOpen::File(hit.path.clone()));
        }
        let content = index.checked_sub(self.names.len())?;
        let hit = self.contents.get(content)?;
        Some(SearchOpen::Match {
            path: hit.path.clone(),
            range: hit.range.clone(),
        })
    }

    fn rows(&self) -> Vec<Row> {
        if !self.names_done && self.names.is_empty() {
            return vec![Row::Status("Searching…".into())];
        }
        if self.names_done && self.content_done && self.names.is_empty() && self.contents.is_empty()
        {
            return vec![Row::Status("No matches".into())];
        }
        let mut rows = Vec::new();
        rows.push(Row::Header(format!("Files ({})", self.names.len())));
        if self.names.is_empty() {
            rows.push(Row::Status("No file names".into()));
        }
        for (index, hit) in self.names.iter().enumerate() {
            rows.push(Row::Hit {
                index,
                label: hit.relative.clone(),
            });
        }
        if self.names_done {
            let title = if self.content_done && !self.content_complete {
                format!("In files ({}+)", self.contents.len())
            } else if self.content_done {
                format!("In files ({})", self.contents.len())
            } else {
                "In files".to_string()
            };
            rows.push(Row::Header(title));
            if !self.content_done && self.contents.is_empty() {
                rows.push(Row::Status("Searching file contents…".into()));
            }
            for (index, hit) in self.contents.iter().enumerate() {
                rows.push(Row::Hit {
                    index: self.names.len() + index,
                    label: content_label(hit),
                });
            }
            if self.content_done && self.contents.is_empty() {
                rows.push(Row::Status("No matches in files".into()));
            }
            if self.content_done && !self.content_complete {
                rows.push(Row::Status("More matches are not listed".into()));
            }
        }
        rows
    }

    fn row_of(&self, hit: usize) -> Option<usize> {
        self.rows().iter().position(|row| match row {
            Row::Hit { index, .. } => *index == hit,
            _ => false,
        })
    }
}

impl Drop for FolderSearch {
    fn drop(&mut self) {
        // The worker is blocked in a walk or on the channel. A new generation
        // ends the walk; dropping the sender ends the channel.
        self.cancel.fetch_add(1, Ordering::Release);
    }
}

fn worker(
    jobs: Receiver<Job>,
    reports: Sender<Report>,
    cancel: Arc<AtomicU64>,
    on_result: impl Fn() + Send + 'static,
) {
    while let Ok(mut job) = jobs.recv() {
        while let Ok(newer) = jobs.try_recv() {
            job = newer;
        }
        if cancel.load(Ordering::Acquire) != job.generation {
            continue;
        }
        run_job(&job, &cancel, &reports, &on_result);
    }
}

fn run_job(job: &Job, cancel: &AtomicU64, reports: &Sender<Report>, on_result: &impl Fn()) {
    let live = || cancel.load(Ordering::Acquire) == job.generation;
    if !live() {
        return;
    }
    let finder = match Finder::compile(
        &job.query,
        SearchOptions {
            case_sensitive: job.case_sensitive,
            regex: job.regex,
        },
    ) {
        Ok(finder) => finder,
        Err(message) => {
            let _ = reports.send(Report::Failed {
                generation: job.generation,
                message,
            });
            on_result();
            return;
        }
    };
    let notes = match walk_notes(&job.root, job.show_all, Some((cancel, job.generation))) {
        Ok(notes) => notes,
        Err(error) if error.kind() == ErrorKind::Interrupted => return,
        Err(error) => {
            let _ = reports.send(Report::Failed {
                generation: job.generation,
                message: error.to_string(),
            });
            on_result();
            return;
        }
    };
    if !live() {
        return;
    }
    // Names go out before any file is read, so the list can show them while
    // the content pass is still walking.
    let _ = reports.send(Report::Names {
        generation: job.generation,
        hits: filename_hits(&notes, &finder),
    });
    on_result();
    if !live() {
        return;
    }
    let Some(found) = content_hits(&notes, &finder, MATCH_CAP, &|| !live()) else {
        return;
    };
    if !live() {
        return;
    }
    let _ = reports.send(Report::Content {
        generation: job.generation,
        hits: found.hits,
        complete: found.complete,
    });
    on_result();
}

fn filename_hits(notes: &[NoteFile], finder: &Finder) -> Vec<NameHit> {
    notes
        .iter()
        .filter(|note| name_matches(finder, note))
        .map(|note| NameHit {
            path: note.path.clone(),
            relative: note.relative.clone(),
        })
        .collect()
}

fn name_matches(finder: &Finder, note: &NoteFile) -> bool {
    if text_matches(finder, &note.name) {
        return true;
    }
    note.relative != note.name && text_matches(finder, &note.relative)
}

fn text_matches(finder: &Finder, text: &str) -> bool {
    let doc = Document::from_text(text);
    finder.next(doc.rope(), 0).is_some()
}

/// `None` when `stopped` fires, so a replaced query does not publish a partial list.
fn content_hits(
    notes: &[NoteFile],
    finder: &Finder,
    limit: usize,
    stopped: &dyn Fn() -> bool,
) -> Option<ContentList> {
    let mut hits = Vec::new();
    let mut complete = true;
    for note in notes {
        if stopped() {
            return None;
        }
        let room = limit.saturating_sub(hits.len());
        if room == 0 {
            complete = false;
            break;
        }
        let Some(doc) = read_note(&note.path) else {
            continue;
        };
        let batch = finder.first_matches(doc.rope(), room + 1);
        let extra = batch.len() > room;
        for range in batch.into_iter().take(room) {
            let preview = preview(&doc, &range);
            hits.push(ContentHit {
                path: note.path.clone(),
                relative: note.relative.clone(),
                range,
                preview,
            });
        }
        if extra {
            complete = false;
            break;
        }
    }
    Some(ContentList { hits, complete })
}

fn read_note(path: &Path) -> Option<Document> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return None;
    }
    Document::open(path).ok()
}

fn preview(doc: &Document, range: &Range<usize>) -> String {
    let at = range.start.min(doc.len());
    let line = doc.byte_to_line(at);
    let span = doc.line_range(line);
    let end = span.end.min(doc.len());
    let start = span.start.min(end);
    let text = doc.slice(start..end);
    let text = text.as_ref();
    const MAX: usize = 80;
    let count = text.chars().count();
    if count <= MAX {
        return text.trim().to_string();
    }
    let local = text
        .get(..at.saturating_sub(start))
        .map(|prefix| prefix.chars().count())
        .unwrap_or(0);
    let from = local.saturating_sub(MAX / 2);
    let snippet: String = text.chars().skip(from).take(MAX).collect();
    let snippet = snippet.trim();
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.push_str(snippet);
    if from + MAX < count {
        out.push('…');
    }
    out
}

fn content_label(hit: &ContentHit) -> String {
    if hit.preview.is_empty() {
        hit.relative.clone()
    } else {
        format!("{}  {}", hit.relative, hit.preview)
    }
}

fn fit_label(
    ui: &egui::Ui,
    text: String,
    font: FontId,
    color: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text,
        egui::TextFormat {
            font_id: font,
            color,
            ..Default::default()
        },
    );
    job.wrap.max_width = max_width.max(0.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.painter().layout_job(job)
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
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn options(case_sensitive: bool) -> SearchOptions {
        SearchOptions {
            case_sensitive,
            regex: false,
        }
    }

    #[test]
    fn a_name_matches_the_file_name_or_its_path() {
        let notes = vec![NoteFile {
            name: "Yeast.md".into(),
            relative: "recipes/Yeast.md".into(),
            path: PathBuf::from("recipes/Yeast.md"),
        }];
        let finder = Finder::compile("yeast", options(false)).unwrap();
        assert_eq!(filename_hits(&notes, &finder).len(), 1);
        let finder = Finder::compile("recipes/yeast", options(false)).unwrap();
        assert_eq!(
            filename_hits(&notes, &finder)[0].relative,
            "recipes/Yeast.md"
        );
        let finder = Finder::compile("yeast", options(true)).unwrap();
        assert!(filename_hits(&notes, &finder).is_empty());
    }

    #[test]
    fn a_crlf_file_is_searched_in_document_bytes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("note.md"), b"a\r\nb\r\n").unwrap();
        let notes = walk_notes(dir.path(), false, None).unwrap();
        let finder = Finder::compile("b", options(true)).unwrap();
        let found = content_hits(&notes, &finder, 10, &|| false).unwrap();
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.hits[0].range, 2..3);
        assert_eq!(found.hits[0].preview, "b");
        assert!(found.complete);
    }

    #[test]
    fn content_hits_keep_the_first_matches_and_stop() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("n.md"), "a a a a\n").unwrap();
        let notes = walk_notes(dir.path(), false, None).unwrap();
        let finder = Finder::compile("a", options(true)).unwrap();
        let found = content_hits(&notes, &finder, 2, &|| false).unwrap();
        assert_eq!(found.hits.len(), 2);
        assert!(!found.complete);
        assert!(content_hits(&notes, &finder, 10, &|| true).is_none());
    }
}
