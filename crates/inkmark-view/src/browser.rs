//! The folder sidebar: a virtualized tree over [`inkmark_files::Tree`].
//! Directory listings run on a worker thread; a frame only paints the rows
//! in view. Expanded folders are watched, so a create, rename or delete
//! shows up without Refresh.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use egui::{
    Align2, EventFilter, FontId, Id, Key, Modifiers, Rect, Response, RichText, ScrollArea, Sense,
    Ui, pos2, vec2,
};
use inkmark_files::{Entry, Pending, Row, Tree, Watch, list_dir};

use crate::theme::{self, Theme};

const ROW_H: f32 = 22.0;

#[derive(Default)]
pub struct BrowserOutput {
    /// A Markdown file the user asked to open. The app runs this through the
    /// same unsaved-changes prompt as Ctrl+O.
    pub open_file: Option<PathBuf>,
    /// The Open Folder… button. The app shows the portal folder picker.
    pub open_folder: bool,
    /// New file, in the selected folder or the root.
    pub new_file: bool,
    /// Rename… (F2 or the context menu). The app asks for the new name.
    pub rename: Option<PathBuf>,
    /// Move to Trash (Delete or the context menu). The app confirms first.
    pub trash: Option<PathBuf>,
    /// Move to… The app shows the portal folder picker.
    pub move_to: Option<PathBuf>,
    /// A row dropped onto a folder: (what, into which folder).
    pub dropped: Option<(PathBuf, PathBuf)>,
}

/// A row being dragged, as egui's drag-and-drop payload.
struct Dragged(PathBuf);

/// A context-menu choice, applied after the rows are drawn.
enum MenuAction {
    Rename(PathBuf),
    MoveTo(PathBuf),
    Trash(PathBuf),
    NewFileIn(PathBuf),
}

struct Listed {
    ticket: u64,
    index: usize,
    generation: u64,
    result: io::Result<Vec<Entry>>,
}

enum Job {
    List(Pending, u64),
    Stop,
}

pub struct FileBrowser {
    tree: Tree,
    tx: Sender<Job>,
    rx: Receiver<Listed>,
    /// Bumped when the root changes so a listing already in flight is dropped.
    epoch: Arc<AtomicU64>,
    ticket: u64,
    inflight: HashSet<(usize, u64)>,
    selected: Option<PathBuf>,
    current: Option<PathBuf>,
    focus_next: bool,
    /// The open file was scrolled into view for the current path.
    revealed: bool,
    id: Id,
    painted: usize,
    scroll_to: Option<f32>,
    row_rects: HashMap<PathBuf, Rect>,
    up_rect: Option<Rect>,
    open_folder_rect: Option<Rect>,
    refresh_rect: Option<Rect>,
    new_file_rect: Option<Rect>,
    watch: Option<Watch>,
    /// Colors, refreshed from the context every frame.
    theme: std::sync::Arc<Theme>,
}

impl FileBrowser {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let (tx, rx, epoch) = spawn_lister();
        Self {
            tree: Tree::new(root),
            tx,
            rx,
            epoch,
            ticket: 1,
            inflight: HashSet::new(),
            selected: None,
            current: None,
            focus_next: false,
            revealed: false,
            id: Id::new("file_browser"),
            painted: 0,
            scroll_to: None,
            row_rects: HashMap::new(),
            up_rect: None,
            open_folder_rect: None,
            refresh_rect: None,
            new_file_rect: None,
            watch: None,
            theme: std::sync::Arc::new(Theme::dark()),
        }
    }

    pub fn root(&self) -> &Path {
        self.tree.root()
    }

    pub fn set_root(&mut self, root: PathBuf) {
        self.tree.set_root(root);
        self.selected = None;
        self.scroll_to = Some(0.0);
        self.revealed = false;
        self.bump_epoch();
    }

    pub fn go_up(&mut self) {
        if let Some(parent) = self.tree.parent_root() {
            self.set_root(parent);
        }
    }

    /// Highlights `path` and expands its parents once they are listed.
    pub fn set_current(&mut self, path: Option<PathBuf>) {
        if self.current == path {
            return;
        }
        self.current = path.clone();
        self.revealed = false;
        if let Some(path) = path {
            self.tree.reveal(&path);
            self.selected = Some(path);
        }
    }

    pub fn request_focus(&mut self) {
        self.focus_next = true;
    }

    pub fn has_focus(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| m.has_focus(self.id))
    }

    pub fn row_count(&mut self) -> usize {
        self.tree.rows().len()
    }

    pub fn row_names(&mut self) -> Vec<String> {
        self.tree
            .rows()
            .iter()
            .map(|row| row.name.clone())
            .collect()
    }

    /// Where a row was drawn last frame, if it was in view.
    pub fn row_rect(&self, path: &Path) -> Option<Rect> {
        self.row_rects.get(path).copied()
    }

    pub fn up_rect(&self) -> Option<Rect> {
        self.up_rect
    }

    pub fn open_folder_rect(&self) -> Option<Rect> {
        self.open_folder_rect
    }

    pub fn refresh_rect(&self) -> Option<Rect> {
        self.refresh_rect
    }

    pub fn new_file_rect(&self) -> Option<Rect> {
        self.new_file_rect
    }

    /// Folder a new file would be created in: the selected folder, the
    /// parent of the selected file, or the root.
    pub fn new_file_dir(&mut self) -> PathBuf {
        let Some(path) = self.selected.clone() else {
            return self.tree.root().to_path_buf();
        };
        if let Some(row) = self.snaps().into_iter().find(|row| row.path == path) {
            if row.kind.is_dir() {
                return row.path;
            }
            if let Some(parent) = row.path.parent() {
                return parent.to_path_buf();
            }
        } else if path.is_dir() {
            return path;
        } else if let Some(parent) = path.parent() {
            return parent.to_path_buf();
        }
        self.tree.root().to_path_buf()
    }

    /// Selects `path` (e.g. after a rename), once its row is listed.
    pub fn select(&mut self, path: &Path) {
        self.selected = Some(path.to_path_buf());
    }

    /// Something in `dir` was created, renamed, moved or removed by the
    /// app: re-read it now rather than waiting for the watcher.
    pub fn refresh_dir(&mut self, dir: &Path) {
        for index in self.tree.dirs_at(dir) {
            self.tree.invalidate(index);
        }
    }

    /// The file was just created here: re-read its folder and reveal it.
    pub fn note_created(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            let indexes = self.tree.dirs_at(parent);
            for index in indexes {
                self.tree.invalidate(index);
            }
        }
        self.set_current(Some(path.to_path_buf()));
    }

    /// Rows painted last frame. A large folder stays small here.
    pub fn painted(&self) -> usize {
        self.painted
    }

    /// Picks up worker listings. `show` does this too; the app also calls it
    /// while the sidebar is hidden so a later open isn't a frame behind.
    pub fn poll_listings(&mut self, ctx: &egui::Context) {
        self.poll(ctx);
    }

    pub fn show(&mut self, ui: &mut Ui) -> BrowserOutput {
        self.theme = theme::current(ui.ctx());
        self.poll(ui.ctx());
        if self.focus_next {
            ui.memory_mut(|m| m.request_focus(self.id));
            self.focus_next = false;
        }
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                self.id,
                EventFilter {
                    tab: false,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: false,
                },
            );
        });
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, self.theme.background);
        // Clicks on empty space focus the tree without moving the caret in a pane.
        let background = ui.interact(rect, self.id, Sense::click());
        if background.clicked() {
            background.request_focus();
        }

        let mut output = BrowserOutput::default();
        self.header(ui, &mut output);
        self.keys(ui, &mut output);
        self.rows_ui(ui, &mut output);
        // Dropped on empty space below the rows: into the root. A row
        // under the pointer has already taken the payload.
        if let Some(dragged) = background.dnd_release_payload::<Dragged>() {
            output.dropped = Some((dragged.0.clone(), self.tree.root().to_path_buf()));
        }
        output
    }

    fn bump_epoch(&mut self) {
        self.ticket = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.inflight.clear();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        self.poll_watch(ctx);
        while let Ok(done) = self.rx.try_recv() {
            self.inflight.remove(&(done.index, done.generation));
            if done.ticket == self.ticket {
                self.tree.apply(done.index, done.generation, done.result);
            }
        }
        let pending = self.tree.pending();
        let mut loops = Vec::new();
        for job in &pending {
            if self.tree.is_loop(job.index) {
                loops.push((job.index, job.generation));
            }
        }
        for (index, generation) in loops {
            self.tree.mark_loop(index, generation);
        }
        for job in self.tree.pending() {
            let key = (job.index, job.generation);
            if !self.inflight.insert(key) {
                continue;
            }
            if self.tx.send(Job::List(job, self.ticket)).is_err() {
                self.inflight.remove(&key);
            }
        }
        if !self.inflight.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
        self.reveal_if_listed();
    }

    fn poll_watch(&mut self, ctx: &egui::Context) {
        if self.watch.is_none() {
            let ctx = ctx.clone();
            self.watch = Watch::new(move || ctx.request_repaint()).ok();
        }
        let Some(watch) = &mut self.watch else {
            return;
        };
        let dirs = self.tree.expanded_dirs();
        watch.sync(&dirs);
        let changed = watch.changed();
        let mut indexes = Vec::new();
        for path in &changed {
            indexes.extend(self.tree.dirs_at(path));
        }
        for index in indexes {
            self.tree.invalidate(index);
        }
        // A backup wake: the watch thread also requests a repaint, but a
        // missed one would otherwise wait on the disk-check interval.
        if !dirs.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    fn reveal_if_listed(&mut self) {
        if self.revealed {
            return;
        }
        let Some(path) = self.current.clone() else {
            return;
        };
        if let Some(index) = self.tree.rows().iter().position(|row| row.path == path) {
            self.selected = Some(path);
            self.scroll_to = Some(index as f32 * ROW_H);
            self.revealed = true;
        }
    }

    fn header(&mut self, ui: &mut Ui, output: &mut BrowserOutput) {
        let name = self.tree.root_name().to_string();
        ui.add_space(4.0);
        ui.label(RichText::new(name).strong().color(self.theme.text));
        // Wrapped, so a narrow sidebar doesn't spill the buttons over the panes.
        ui.horizontal_wrapped(|ui| {
            let can_up = self.tree.parent_root().is_some();
            let up = ui.add_enabled(can_up, egui::Button::new("Up"));
            self.up_rect = Some(up.rect);
            if up.clicked() {
                self.go_up();
            }
            let open = ui.button("Open Folder…");
            self.open_folder_rect = Some(open.rect);
            if open.clicked() {
                output.open_folder = true;
            }
            let refresh = ui.button("Refresh");
            self.refresh_rect = Some(refresh.rect);
            if refresh.clicked() {
                self.refresh();
            }
            let new_file = ui.button("New file");
            self.new_file_rect = Some(new_file.rect);
            if new_file.clicked() {
                output.new_file = true;
            }
            let mut show_all = self.tree.show_all();
            if ui.checkbox(&mut show_all, "All files").changed() {
                self.tree.set_show_all(show_all);
            }
        });
        ui.separator();
    }

    fn refresh(&mut self) {
        let dirs = self.tree.expanded_dirs();
        let mut indexes = Vec::new();
        for path in &dirs {
            indexes.extend(self.tree.dirs_at(path));
        }
        for index in indexes {
            self.tree.invalidate(index);
        }
    }

    fn keys(&mut self, ui: &mut Ui, output: &mut BrowserOutput) {
        if !ui.memory(|m| m.has_focus(self.id)) {
            return;
        }
        let (up, down, left, right, enter, rename, delete) = ui.input_mut(|input| {
            (
                input.consume_key(Modifiers::NONE, Key::ArrowUp),
                input.consume_key(Modifiers::NONE, Key::ArrowDown),
                input.consume_key(Modifiers::NONE, Key::ArrowLeft),
                input.consume_key(Modifiers::NONE, Key::ArrowRight),
                input.consume_key(Modifiers::NONE, Key::Enter),
                input.consume_key(Modifiers::NONE, Key::F2),
                input.consume_key(Modifiers::NONE, Key::Delete),
            )
        });
        if let Some(path) = self.selected_path() {
            if rename {
                output.rename = Some(path.clone());
            }
            if delete {
                output.trash = Some(path);
            }
        }
        if up {
            self.move_by(-1);
        }
        if down {
            self.move_by(1);
        }
        if left {
            self.collapse_or_parent();
        }
        if right {
            self.expand_or_child();
        }
        if enter {
            self.activate(output);
        }
    }

    /// The selected row's path, if it's still in the tree.
    fn selected_path(&mut self) -> Option<PathBuf> {
        let pos = self.selected_pos()?;
        Some(self.tree.rows()[pos].path.clone())
    }

    /// The folder a row dropped on `row` goes into: the row itself if it's
    /// a folder, else the folder it's in.
    fn drop_dir(row: &Row) -> Option<PathBuf> {
        if row.kind.is_dir() && !row.kind.looped() {
            Some(row.path.clone())
        } else {
            row.path.parent().map(Path::to_path_buf)
        }
    }

    fn snaps(&mut self) -> Vec<Row> {
        self.tree.rows().to_vec()
    }

    fn selected_pos(&mut self) -> Option<usize> {
        let selected = self.selected.clone()?;
        self.tree.rows().iter().position(|row| row.path == selected)
    }

    fn move_by(&mut self, delta: isize) {
        let rows = self.snaps();
        if rows.is_empty() {
            return;
        }
        let next = match self.selected_pos() {
            None => {
                if delta < 0 {
                    rows.len() - 1
                } else {
                    0
                }
            }
            Some(index) => (index as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        };
        self.selected = Some(rows[next].path.clone());
        self.scroll_to = Some(next as f32 * ROW_H);
    }

    fn collapse_or_parent(&mut self) {
        let Some(pos) = self.selected_pos() else {
            return;
        };
        let row = self.snaps()[pos].clone();
        if row.kind.is_dir() && row.expanded {
            self.tree.collapse(row.index);
            return;
        }
        if row.depth == 0 {
            return;
        }
        let depth = row.depth;
        let rows = self.snaps();
        if let Some((index, parent)) = rows[..pos]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, candidate)| candidate.depth + 1 == depth)
        {
            self.selected = Some(parent.path.clone());
            self.scroll_to = Some(index as f32 * ROW_H);
        }
    }

    fn expand_or_child(&mut self) {
        let Some(pos) = self.selected_pos() else {
            return;
        };
        let row = self.snaps()[pos].clone();
        if !row.kind.is_dir() || row.kind.looped() {
            return;
        }
        if !row.expanded {
            self.tree.set_expanded(row.index, true);
            return;
        }
        let rows = self.snaps();
        if rows.get(pos + 1).is_some_and(|next| next.depth > row.depth) {
            self.selected = Some(rows[pos + 1].path.clone());
        }
    }

    fn activate(&mut self, output: &mut BrowserOutput) {
        let Some(pos) = self.selected_pos() else {
            return;
        };
        let row = self.snaps()[pos].clone();
        self.activate_row(&row, output);
    }

    fn activate_row(&mut self, row: &Row, output: &mut BrowserOutput) {
        if row.kind.openable() {
            output.open_file = Some(row.path.clone());
        } else if row.kind.is_dir() && !row.kind.looped() {
            self.tree.set_expanded(row.index, !row.expanded);
        }
    }

    fn rows_ui(&mut self, ui: &mut Ui, output: &mut BrowserOutput) {
        let rows = self.snaps();
        let scroll_to = self.scroll_to.take();
        self.row_rects.clear();
        self.painted = 0;
        let mut area = ScrollArea::vertical()
            .auto_shrink([false, false])
            .id_salt(self.id);
        let mut clicked = None;
        let mut menu = None;
        let mut dropped = None;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            // Compare the row with the range on screen, not with the height
            // alone. A row above the window has a small absolute offset, and
            // a row one past the bottom should move by one row.
            if let Some(row_y) = scroll_to {
                let viewport = ui.available_height();
                if viewport <= ROW_H {
                    self.scroll_to = Some(row_y);
                } else {
                    // ScrollArea salts the id again, so load the same `IdSalt`.
                    let scroll_id = ui.make_persistent_id(egui::IdSalt::new(self.id));
                    let current = egui::containers::scroll_area::State::load(ui.ctx(), scroll_id)
                        .map(|state| state.offset.y)
                        .unwrap_or(0.0);
                    if let Some(offset) = offset_to_reveal(row_y, current, viewport) {
                        area = area.vertical_scroll_offset(offset);
                    }
                }
            }
            area.show_rows(ui, ROW_H, rows.len(), |ui, range| {
                self.painted = range.len();
                for index in range {
                    let row = &rows[index];
                    if let Some(response) = self.paint_row(ui, row) {
                        self.row_rects.insert(row.path.clone(), response.rect);
                        if response.clicked() {
                            clicked = Some(index);
                        }
                        response.dnd_set_drag_payload(Dragged(row.path.clone()));
                        let target = Self::drop_dir(row);
                        // A row can't go into itself or where it already is.
                        let accepts = |dragged: &Dragged| {
                            target.as_ref().is_some_and(|dir| {
                                !dir.starts_with(&dragged.0) && dragged.0.parent() != Some(dir)
                            })
                        };
                        if let Some(dragged) = response.dnd_hover_payload::<Dragged>()
                            && accepts(&dragged)
                        {
                            ui.painter().rect_stroke(
                                response.rect.shrink(1.0),
                                2.0,
                                egui::Stroke::new(1.5, self.theme.caret),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if let Some(dragged) = response.dnd_release_payload::<Dragged>()
                            && accepts(&dragged)
                            && let Some(dir) = target.clone()
                        {
                            dropped = Some((dragged.0.clone(), dir));
                        }
                        response.context_menu(|ui| {
                            let path = row.path.clone();
                            if ui.button("Rename…").clicked() {
                                menu = Some(MenuAction::Rename(path.clone()));
                                ui.close();
                            }
                            if ui.button("Move to…").clicked() {
                                menu = Some(MenuAction::MoveTo(path.clone()));
                                ui.close();
                            }
                            if ui.button("Move to Trash").clicked() {
                                menu = Some(MenuAction::Trash(path.clone()));
                                ui.close();
                            }
                            if let Some(dir) = Self::drop_dir(row)
                                && ui.button("New file here…").clicked()
                            {
                                menu = Some(MenuAction::NewFileIn(dir));
                                ui.close();
                            }
                        });
                    }
                }
            });
        });
        if let Some(index) = clicked {
            let row = rows[index].clone();
            self.selected = Some(row.path.clone());
            ui.memory_mut(|m| m.request_focus(self.id));
            self.activate_row(&row, output);
        }
        output.dropped = output.dropped.take().or(dropped);
        match menu {
            Some(MenuAction::Rename(path)) => output.rename = Some(path),
            Some(MenuAction::MoveTo(path)) => output.move_to = Some(path),
            Some(MenuAction::Trash(path)) => output.trash = Some(path),
            Some(MenuAction::NewFileIn(dir)) => {
                // New file goes into the selected folder.
                self.selected = Some(dir);
                output.new_file = true;
            }
            None => {}
        }
    }

    fn paint_row(&self, ui: &mut Ui, row: &Row) -> Option<Response> {
        let width = ui.available_width();
        if !width.is_finite() {
            return None;
        }
        // Not focusable: arrow keys move egui focus to the next focusable
        // widget, which would take the tree's keys after one press.
        let (rect, response) =
            ui.allocate_exact_size(vec2(width, ROW_H), Sense::CLICK | Sense::DRAG);
        let selected = self.selected.as_deref() == Some(row.path.as_path());
        let current = self.current.as_deref() == Some(row.path.as_path());
        if selected {
            ui.painter().rect_filled(rect, 0.0, self.theme.selection);
        } else if current {
            ui.painter().rect_filled(rect, 0.0, self.theme.current_row);
        }
        if current {
            ui.painter().rect_filled(
                Rect::from_min_max(rect.min, pos2(rect.left() + 2.0, rect.bottom())),
                0.0,
                self.theme.caret,
            );
        }
        let indent = rect.left() + 8.0 + row.depth as f32 * 14.0;
        let marker = if row.kind.is_dir() {
            if row.expanded { "▾" } else { "▸" }
        } else {
            " "
        };
        let mut label = row.name.clone();
        if row.kind.unreadable() {
            label.push_str("  unreadable");
        } else if row.kind.looped() {
            label.push_str("  loop");
        }
        let color = if row.kind.is_dir() || row.kind.openable() {
            self.theme.text
        } else {
            self.theme.markup
        };
        ui.painter().text(
            pos2(indent, rect.center().y),
            Align2::LEFT_CENTER,
            marker,
            FontId::proportional(13.0),
            self.theme.markup,
        );
        ui.painter().text(
            pos2(indent + 16.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(13.0),
            color,
        );
        Some(response)
    }
}

impl Drop for FileBrowser {
    fn drop(&mut self) {
        let _ = self.tx.send(Job::Stop);
    }
}

/// Smallest offset that puts `[row_y, row_y + ROW_H]` inside the window.
/// `None` when it is already fully visible, so a visible row is not pinned
/// to the top (that scrolls the rows above it under the header).
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

fn spawn_lister() -> (Sender<Job>, Receiver<Listed>, Arc<AtomicU64>) {
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let (done_tx, done_rx) = mpsc::channel();
    let epoch = Arc::new(AtomicU64::new(1));
    let epoch_worker = Arc::clone(&epoch);
    std::thread::spawn(move || {
        while let Ok(job) = job_rx.recv() {
            match job {
                Job::Stop => break,
                Job::List(pending, ticket) => {
                    let Pending {
                        index,
                        generation,
                        path,
                        show_all,
                    } = pending;
                    // read_dir of a large folder must not run on the UI thread.
                    // The epoch aborts it when the root changes mid-listing.
                    let result = list_dir(&path, show_all, Some((&epoch_worker, ticket)));
                    if done_tx
                        .send(Listed {
                            ticket,
                            index,
                            generation,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    (job_tx, done_rx, epoch)
}
