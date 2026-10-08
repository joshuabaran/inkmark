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

/// ScrollArea salt for the result list. Arrow-key scrolling reads the same id.
const LIST_ID: &str = "folder_search";

/// What the user asked to open from a result row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SearchOpen {
    File(PathBuf),
    Match {
        path: PathBuf,
        range: Range<usize>,
        /// The bytes the match covered when it was found.
        text: String,
    },
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
    /// The matched bytes. A later edit can move the range; the text stays.
    text: String,
    preview: String,
}

#[cfg(test)]
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
        /// This batch is the last one for the pass.
        done: bool,
        complete: bool,
    },
    Failed {
        generation: u64,
        message: String,
    },
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

    /// Ctrl+Shift+F. A query already typed is searched again, so a note
    /// added while the bar was closed is included.
    pub(crate) fn open(&mut self) {
        self.open = true;
        self.pending_focus = true;
        self.select_query = !self.query.is_empty();
        if !self.query.is_empty() {
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
        let shape = self.shape();
        let layout = list_layout(&shape);
        let scroll_to = self.scroll_to.take();
        let selected = self.selected;
        let clicked = ui
            .scope(|ui| {
                // The file tree does this too. Row stride is then `ROW_H`,
                // which is what the arrow keys scroll by.
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut area = ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .id_salt(LIST_ID);
                if let Some(index) = scroll_to {
                    let viewport = ui.available_height();
                    if viewport <= ROW_H {
                        self.scroll_to = Some(index);
                    } else {
                        let scroll_id = ui.make_persistent_id(egui::IdSalt::new(LIST_ID));
                        let current =
                            egui::containers::scroll_area::State::load(ui.ctx(), scroll_id)
                                .map(|state| state.offset.y)
                                .unwrap_or(0.0);
                        if let Some(offset) =
                            offset_to_reveal(index as f32 * ROW_H, current, viewport)
                        {
                            area = area.vertical_scroll_offset(offset);
                        }
                    }
                }
                let mut clicked = None;
                area.show_rows(ui, ROW_H, layout.count, |ui, range| {
                    for index in range {
                        let Some(slot) = slot_at(&layout, index) else {
                            continue;
                        };
                        let width = ui.available_width();
                        if !width.is_finite() {
                            continue;
                        }
                        let hit = match slot {
                            Slot::Hit(hit) => Some(hit),
                            _ => None,
                        };
                        let sense = if hit.is_some() {
                            Sense::click()
                        } else {
                            Sense::hover()
                        };
                        let (rect, response) =
                            ui.allocate_exact_size(egui::vec2(width, ROW_H), sense);
                        if hit.is_some_and(|hit| hit == selected) {
                            ui.painter().rect_filled(rect, 0.0, colors.selection);
                        }
                        let color = if hit.is_some() {
                            colors.text
                        } else {
                            colors.markup
                        };
                        let galley = fit_label(
                            ui,
                            self.slot_label(&shape, slot),
                            FontId::proportional(13.0),
                            color,
                            (rect.width() - 16.0).max(0.0),
                        );
                        let pos =
                            egui::pos2(rect.left() + 8.0, rect.center().y - galley.size().y * 0.5);
                        ui.painter().galley(pos, galley, color);
                        if response.clicked()
                            && let Some(hit) = hit
                        {
                            clicked = Some(hit);
                        }
                    }
                });
                clicked
            })
            .inner;
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
                    done,
                    complete,
                } if generation == self.generation => {
                    self.contents.extend(hits);
                    if done {
                        self.content_done = true;
                        self.content_complete = complete;
                        self.scanning = false;
                    }
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
            text: hit.text.clone(),
        })
    }

    fn shape(&self) -> Shape {
        Shape {
            names_done: self.names_done,
            content_done: self.content_done,
            content_complete: self.content_complete,
            names: self.names.len(),
            contents: self.contents.len(),
        }
    }

    fn row_of(&self, hit: usize) -> Option<usize> {
        hit_row(&list_layout(&self.shape()), hit)
    }

    fn slot_label(&self, shape: &Shape, slot: Slot) -> String {
        match slot {
            Slot::Searching => "Searching…".into(),
            Slot::NoMatches => "No matches".into(),
            Slot::FilesHeader => format!("Files ({})", shape.names),
            Slot::NoFileNames => "No file names".into(),
            Slot::InFilesHeader => {
                if !shape.content_done && shape.contents == 0 {
                    "In files".into()
                } else if shape.content_done && !shape.content_complete {
                    format!("In files ({}+)", shape.contents)
                } else {
                    format!("In files ({})", shape.contents)
                }
            }
            Slot::SearchingContents => "Searching file contents…".into(),
            Slot::NoContentMatches => "No matches in files".into(),
            Slot::MoreNotListed => "More matches are not listed".into(),
            Slot::Hit(index) => self.hit_label(index),
        }
    }

    fn hit_label(&self, index: usize) -> String {
        if let Some(hit) = self.names.get(index) {
            return hit.relative.clone();
        }
        self.contents
            .get(index - self.names.len())
            .map(content_label)
            .unwrap_or_default()
    }

    /// Where `text` sits in `doc` now. The recorded range wins when those
    /// bytes are still that match. Otherwise the nearest match of the same
    /// text is used. `None` when that text is no longer a match, so a click
    /// does not select some other span.
    pub(crate) fn locate(
        &self,
        doc: &Document,
        range: Range<usize>,
        text: &str,
    ) -> Option<Range<usize>> {
        let finder = Finder::compile(&self.query, self.options()).ok()?;
        locate_match(doc, &finder, range, text)
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
    // Each file's matches go out as they are read. The last batch says the
    // pass is finished, including when nothing matched.
    let _ = scan_contents(&notes, &finder, MATCH_CAP, &|| !live(), &mut |batch| {
        if !live() {
            return;
        }
        let _ = reports.send(Report::Content {
            generation: job.generation,
            hits: batch.hits,
            done: batch.done,
            complete: batch.complete,
        });
        on_result();
    });
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

struct ContentBatch {
    hits: Vec<ContentHit>,
    done: bool,
    complete: bool,
}

/// `None` when `stopped` fires. Each file that has matches is one batch, and
/// the last batch has `done` set, so the list can grow while the walk continues.
fn scan_contents(
    notes: &[NoteFile],
    finder: &Finder,
    limit: usize,
    stopped: &dyn Fn() -> bool,
    emit: &mut dyn FnMut(ContentBatch),
) -> Option<()> {
    let mut total = 0usize;
    let mut complete = true;
    for note in notes {
        if stopped() {
            return None;
        }
        let room = limit.saturating_sub(total);
        if room == 0 {
            complete = false;
            break;
        }
        let Some(doc) = read_note(&note.path) else {
            continue;
        };
        let found = finder.first_matches(doc.rope(), room + 1);
        let extra = found.len() > room;
        let mut hits = Vec::new();
        for range in found.into_iter().take(room) {
            let preview = preview(&doc, &range);
            let text = doc.slice(range.clone()).into_owned();
            hits.push(ContentHit {
                path: note.path.clone(),
                relative: note.relative.clone(),
                range,
                text,
                preview,
            });
        }
        total += hits.len();
        if extra {
            emit(ContentBatch {
                hits,
                done: true,
                complete: false,
            });
            return Some(());
        }
        if !hits.is_empty() {
            emit(ContentBatch {
                hits,
                done: false,
                complete: true,
            });
        }
    }
    emit(ContentBatch {
        hits: Vec::new(),
        done: true,
        complete,
    });
    Some(())
}

/// `None` when `stopped` fires, so a replaced query does not publish a partial list.
#[cfg(test)]
fn content_hits(
    notes: &[NoteFile],
    finder: &Finder,
    limit: usize,
    stopped: &dyn Fn() -> bool,
) -> Option<ContentList> {
    let mut list = ContentList {
        hits: Vec::new(),
        complete: true,
    };
    scan_contents(notes, finder, limit, stopped, &mut |batch| {
        list.hits.extend(batch.hits);
        if batch.done {
            list.complete = batch.complete;
        }
    })?;
    Some(list)
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

struct Shape {
    names_done: bool,
    content_done: bool,
    content_complete: bool,
    names: usize,
    contents: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Searching,
    NoMatches,
    FilesHeader,
    NoFileNames,
    Hit(usize),
    InFilesHeader,
    SearchingContents,
    NoContentMatches,
    MoreNotListed,
}

struct ListLayout {
    count: usize,
    banner: Option<Slot>,
    no_names_at: Option<usize>,
    names_at: Option<usize>,
    name_count: usize,
    in_files_at: Option<usize>,
    searching_at: Option<usize>,
    contents_at: Option<usize>,
    content_count: usize,
    no_content_at: Option<usize>,
    more_at: Option<usize>,
    still_at: Option<usize>,
}

fn list_layout(shape: &Shape) -> ListLayout {
    if !shape.names_done && shape.names == 0 {
        return ListLayout {
            count: 1,
            banner: Some(Slot::Searching),
            ..ListLayout::empty()
        };
    }
    if shape.names_done && shape.content_done && shape.names == 0 && shape.contents == 0 {
        return ListLayout {
            count: 1,
            banner: Some(Slot::NoMatches),
            ..ListLayout::empty()
        };
    }
    let mut count = 1;
    let (names_at, no_names_at) = if shape.names == 0 {
        let at = count;
        count += 1;
        (None, Some(at))
    } else {
        let at = count;
        count += shape.names;
        (Some(at), None)
    };
    let mut in_files_at = None;
    let mut searching_at = None;
    let mut contents_at = None;
    let mut no_content_at = None;
    let mut more_at = None;
    let mut still_at = None;
    if shape.names_done {
        in_files_at = Some(count);
        count += 1;
        if !shape.content_done && shape.contents == 0 {
            searching_at = Some(count);
            count += 1;
        }
        if shape.contents > 0 {
            contents_at = Some(count);
            count += shape.contents;
        }
        if shape.content_done && shape.contents == 0 {
            no_content_at = Some(count);
            count += 1;
        }
        if shape.content_done && !shape.content_complete {
            more_at = Some(count);
            count += 1;
        }
        if !shape.content_done && shape.contents > 0 {
            still_at = Some(count);
            count += 1;
        }
    }
    ListLayout {
        count,
        banner: None,
        no_names_at,
        names_at,
        name_count: shape.names,
        in_files_at,
        searching_at,
        contents_at,
        content_count: shape.contents,
        no_content_at,
        more_at,
        still_at,
    }
}

impl ListLayout {
    fn empty() -> Self {
        Self {
            count: 0,
            banner: None,
            no_names_at: None,
            names_at: None,
            name_count: 0,
            in_files_at: None,
            searching_at: None,
            contents_at: None,
            content_count: 0,
            no_content_at: None,
            more_at: None,
            still_at: None,
        }
    }
}

fn slot_at(layout: &ListLayout, index: usize) -> Option<Slot> {
    if index >= layout.count {
        return None;
    }
    if let Some(slot) = layout.banner {
        return Some(slot);
    }
    if index == 0 {
        return Some(Slot::FilesHeader);
    }
    if layout.no_names_at == Some(index) {
        return Some(Slot::NoFileNames);
    }
    if let Some(at) = layout.names_at
        && index >= at
        && index < at + layout.name_count
    {
        return Some(Slot::Hit(index - at));
    }
    if layout.in_files_at == Some(index) {
        return Some(Slot::InFilesHeader);
    }
    if layout.searching_at == Some(index) || layout.still_at == Some(index) {
        return Some(Slot::SearchingContents);
    }
    if let Some(at) = layout.contents_at
        && index >= at
        && index < at + layout.content_count
    {
        return Some(Slot::Hit(layout.name_count + (index - at)));
    }
    if layout.no_content_at == Some(index) {
        return Some(Slot::NoContentMatches);
    }
    if layout.more_at == Some(index) {
        return Some(Slot::MoreNotListed);
    }
    None
}

fn hit_row(layout: &ListLayout, hit: usize) -> Option<usize> {
    if hit < layout.name_count {
        return layout.names_at.map(|at| at + hit);
    }
    let content = hit - layout.name_count;
    if content < layout.content_count {
        layout.contents_at.map(|at| at + content)
    } else {
        None
    }
}

fn locate_match(
    doc: &Document,
    finder: &Finder,
    range: Range<usize>,
    text: &str,
) -> Option<Range<usize>> {
    if range.end <= doc.len()
        && range.start <= range.end
        && doc.slice(range.clone()).as_ref() == text
        && finder.next(doc.rope(), range.start).as_ref() == Some(&range)
    {
        return Some(range);
    }
    let mut best: Option<Range<usize>> = None;
    let mut best_dist = usize::MAX;
    for candidate in finder.first_matches(doc.rope(), MATCH_CAP) {
        if doc.slice(candidate.clone()).as_ref() != text {
            continue;
        }
        let dist = candidate.start.abs_diff(range.start);
        if dist < best_dist {
            best_dist = dist;
            best = Some(candidate);
        }
    }
    best
}

/// Smallest offset that puts `[row_y, row_y + ROW_H]` inside the window.
/// `None` when that row is already fully visible.
fn offset_to_reveal(row_y: f32, current: f32, viewport: f32) -> Option<f32> {
    let row_bottom = row_y + ROW_H;
    let view_bottom = current + viewport;
    if row_y + 0.5 < current {
        Some(row_y.max(0.0))
    } else if row_bottom > view_bottom + 0.5 {
        Some((row_bottom - viewport).max(0.0))
    } else {
        None
    }
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
        assert_eq!(found.hits[0].text, "b");
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

    #[test]
    fn content_matches_are_reported_as_files_are_read() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "token\n").unwrap();
        fs::write(dir.path().join("b.md"), "token\n").unwrap();
        let notes = walk_notes(dir.path(), false, None).unwrap();
        let finder = Finder::compile("token", options(true)).unwrap();
        let mut batches = 0;
        let mut total = 0;
        let mut finished = false;
        scan_contents(&notes, &finder, 10, &|| false, &mut |batch| {
            if !batch.hits.is_empty() {
                batches += 1;
                total += batch.hits.len();
            }
            if batch.done {
                finished = true;
                assert!(batch.complete);
            }
        })
        .unwrap();
        assert!(batches >= 2);
        assert_eq!(total, 2);
        assert!(finished);
    }

    #[test]
    fn a_moved_match_keeps_its_text_and_a_missing_one_is_dropped() {
        let finder = Finder::compile("token", options(true)).unwrap();
        let same = Document::from_text("see token here\n");
        assert_eq!(locate_match(&same, &finder, 4..9, "token"), Some(4..9));

        let moved = Document::from_text("xxsee token here\n");
        assert_eq!(locate_match(&moved, &finder, 4..9, "token"), Some(6..11));

        let gone = Document::from_text("see nothing here\n");
        assert_eq!(locate_match(&gone, &finder, 4..9, "token"), None);
    }

    #[test]
    fn a_visible_row_stays_and_a_hidden_row_scrolls_to_its_edge() {
        assert_eq!(offset_to_reveal(0.0, 0.0, 100.0), None);
        assert_eq!(offset_to_reveal(22.0, 0.0, 100.0), None);
        assert_eq!(offset_to_reveal(220.0, 0.0, 100.0), Some(142.0));
        assert_eq!(offset_to_reveal(0.0, 50.0, 100.0), Some(0.0));
    }

    fn slots(shape: &Shape) -> Vec<Slot> {
        let layout = list_layout(shape);
        (0..layout.count)
            .map(|index| slot_at(&layout, index).unwrap())
            .collect()
    }

    #[test]
    fn result_rows_follow_the_section_order() {
        assert_eq!(
            slots(&Shape {
                names_done: false,
                content_done: false,
                content_complete: true,
                names: 0,
                contents: 0,
            }),
            vec![Slot::Searching]
        );
        assert_eq!(
            slots(&Shape {
                names_done: true,
                content_done: true,
                content_complete: true,
                names: 0,
                contents: 0,
            }),
            vec![Slot::NoMatches]
        );
        assert_eq!(
            slots(&Shape {
                names_done: true,
                content_done: false,
                content_complete: true,
                names: 1,
                contents: 1,
            }),
            vec![
                Slot::FilesHeader,
                Slot::Hit(0),
                Slot::InFilesHeader,
                Slot::Hit(1),
                Slot::SearchingContents,
            ]
        );
        let done = Shape {
            names_done: true,
            content_done: true,
            content_complete: false,
            names: 0,
            contents: 2,
        };
        assert_eq!(
            slots(&done),
            vec![
                Slot::FilesHeader,
                Slot::NoFileNames,
                Slot::InFilesHeader,
                Slot::Hit(0),
                Slot::Hit(1),
                Slot::MoreNotListed,
            ]
        );
        let layout = list_layout(&done);
        assert_eq!(hit_row(&layout, 0), Some(3));
        assert_eq!(hit_row(&layout, 1), Some(4));
        assert_eq!(hit_row(&layout, 2), None);
    }
}

/// Folder search on the generated 10,000-note folder. Run with
/// `cargo test --release -p inkmark -- --ignored --nocapture bench_folder_search`,
/// or through `scripts/bench.sh`.
#[cfg(test)]
mod bench {
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;
    use std::time::Instant;

    use inkmark_bench::{Reporter, fixtures};

    use super::*;

    #[test]
    #[ignore]
    fn bench_folder_search_10k() {
        let root = fixtures::dir().join("folder-10k");
        if !root.exists() {
            let notes = if inkmark_bench::quick() {
                2_000
            } else {
                10_000
            };
            fixtures::write_folder(&root, notes, fixtures::SEED).unwrap();
        }
        let reporter = Reporter::new("search");
        for (name, query, regex) in [
            ("folder_rare_word", fixtures::NEEDLE, false),
            ("folder_common_word", "the", false),
            ("folder_regex", r"\b\w+ly\b", true),
        ] {
            let (mut names, mut first, mut done) = (Vec::new(), Vec::new(), Vec::new());
            let mut hits = 0;
            for _ in 0..inkmark_bench::iterations(5, 2) {
                let job = Job {
                    generation: 1,
                    root: root.clone(),
                    query: query.into(),
                    case_sensitive: false,
                    regex,
                    show_all: false,
                };
                let cancel = AtomicU64::new(1);
                let (tx, rx) = mpsc::channel();
                let started = Instant::now();
                let ms = |t: Instant| t.duration_since(started).as_secs_f64() * 1000.0;
                let stamps = std::cell::RefCell::new(Vec::new());
                run_job(&job, &cancel, &tx, &|| {
                    stamps.borrow_mut().push(Instant::now())
                });
                drop(tx);
                let stamps = stamps.into_inner();
                let mut first_content = None;
                hits = 0;
                for (report, at) in rx.iter().zip(&stamps) {
                    match report {
                        Report::Names { .. } => names.push(ms(*at)),
                        Report::Content {
                            hits: h, done: d, ..
                        } => {
                            hits += h.len();
                            if !h.is_empty() && first_content.is_none() {
                                first_content = Some(ms(*at));
                            }
                            if d {
                                done.push(ms(*at));
                            }
                        }
                        Report::Failed { message, .. } => panic!("{message}"),
                    }
                }
                first.extend(first_content);
            }
            reporter.record(&format!("{name}_names"), &names, None, &[]);
            reporter.record(&format!("{name}_first_content"), &first, None, &[]);
            reporter.record(
                &format!("{name}_done"),
                &done,
                None,
                &[("hits", hits as f64)],
            );
        }
    }
}
