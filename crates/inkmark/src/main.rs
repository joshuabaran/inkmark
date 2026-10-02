use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, Key, KeyboardShortcut, Modifiers, Rect, RichText, Stroke, UiBuilder,
    ViewportCommand, pos2,
};
use inkmark_buffer::{DiskStatus, Document, LineEnding, OpenError};
use inkmark_files::{
    Launch, NewFileError, SystemTrash, Trash, choose_root, create_new_file, move_into, rename,
};
use inkmark_parse::{GfmParser, ParseState};
use inkmark_text::Fonts;
use inkmark_view::{BrowserOutput, CodeView, FileBrowser, LiveView};

/// How long a key hint stays in the status bar.
const HINT_TIME: Duration = Duration::from_secs(4);

/// How often we look for changes made to the file by other programs.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd", "txt"];

const OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const SAVE_AS: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S);
const CYCLE_MODE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::E);
const FOCUS_CODE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Num1);
const FOCUS_LIVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Num2);
const TOGGLE_MINIMAP: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::M);
const RECENT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
const OPEN_FOLDER: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::O);
const TOGGLE_BROWSER: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::E);
const NEW_FILE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
const BACK: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Split,
    Code,
    Live,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Code,
    Live,
}

mod links;
mod measure;
mod recent;
mod sidebar;

fn main() -> eframe::Result {
    let start = Instant::now();
    let path = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("inkmark")
            .with_app_id("inkmark")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    // Built with `--features glow`, INKMARK_RENDERER=glow picks OpenGL over wgpu.
    #[cfg(feature = "glow")]
    let options = eframe::NativeOptions {
        renderer: if std::env::var("INKMARK_RENDERER").as_deref() == Ok("glow") {
            eframe::Renderer::Glow
        } else {
            eframe::Renderer::Wgpu
        },
        ..options
    };
    eframe::run_native(
        "inkmark",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            let mut app = App::new(&cc.egui_ctx, path);
            app.measure = measure::Measure::from_env(start);
            Ok(Box::new(app))
        }),
    )
}

/// The open file's state on disk, when it needs the user's attention.
/// Errors are shown separately, so one never hides the other.
enum Banner {
    /// Another program changed the file since we loaded or saved it.
    DiskChanged,
    DiskMissing,
}

/// An action waiting on "discard unsaved changes?".
#[derive(Clone, Debug, PartialEq, Eq)]
enum Confirm {
    /// Ctrl+O: show the open dialog.
    Open,
    /// Open this file (from the recent list).
    OpenPath(PathBuf),
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DialogKind {
    Open,
    SaveAs,
    Folder,
    /// Pick the folder to move this file or folder into.
    MoveTo(PathBuf),
}

enum DialogResult {
    Open(Option<PathBuf>),
    SaveAs(Option<PathBuf>),
    Folder(Option<PathBuf>),
    MoveTo(PathBuf, Option<PathBuf>),
}

/// A place in a document to come back to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Place {
    path: Option<PathBuf>,
    offset: usize,
}

/// What to do once a file opened by following a link is parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Jump {
    Anchor(String),
    Offset(usize),
}

#[derive(Clone)]
struct RenamePrompt {
    path: PathBuf,
    name: String,
    error: Option<String>,
}

#[derive(Clone)]
struct NewFilePrompt {
    dir: PathBuf,
    name: String,
    error: Option<String>,
}

struct App {
    doc: Document,
    code: CodeView,
    live: LiveView,
    parse: ParseState,
    mode: Mode,
    /// The pane with keyboard focus (or that last had it).
    focus: Pane,
    banner: Option<Banner>,
    /// The last thing that failed (open, save, a dialog), until dismissed.
    error: Option<String>,
    /// A pane's explanation for a key that did nothing, shown for a moment.
    hint: Option<(String, Instant)>,
    /// Where followed links were clicked, most recent last (Alt+Left).
    back: Vec<Place>,
    /// Where to put the caret once the file being opened has been parsed.
    pending_jump: Option<(PathBuf, Jump)>,
    /// Tests record URLs here instead of starting a browser.
    #[cfg(test)]
    opened_urls: Vec<String>,
    dialog: Option<Receiver<DialogResult>>,
    confirm: Option<Confirm>,
    close_after_save: bool,
    close_allowed: bool,
    next_disk_check: Instant,
    title: String,
    measure: Option<measure::Measure>,
    recent: recent::Recent,
    /// The recent-files list is open, with this entry selected.
    recent_list: Option<usize>,
    browser: FileBrowser,
    sidebar: sidebar::Sidebar,
    /// Asks for a name, then creates the file and opens it.
    new_file: Option<NewFilePrompt>,
    /// Asks for a new name for this file or folder.
    rename: Option<RenamePrompt>,
    /// Asks before moving this file or folder to the trash.
    trash_confirm: Option<PathBuf>,
    /// Where Move to Trash sends things; tests use their own.
    trash: Box<dyn Trash>,
    /// A dialog took keyboard focus from the panes last frame.
    modal_was_open: bool,
    /// Tests receive the dialog kind instead of opening a portal window.
    #[cfg(test)]
    dialog_hook: Option<mpsc::Sender<DialogKind>>,
}

impl App {
    fn new(ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        Self::with_recent(ctx, path, recent::Recent::load())
    }

    fn with_recent(ctx: &egui::Context, path: Option<PathBuf>, recent: recent::Recent) -> Self {
        // One font database and glyph atlas for both panes.
        let fonts = Fonts::shared(ctx);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let launch = choose_root(path.as_deref(), &cwd);
        let root = match &launch {
            Launch::File { root, .. } | Launch::Folder { root } => root.clone(),
        };
        let sidebar_store = recent
            .store_path()
            .and_then(|store| store.parent().map(|dir| dir.join("sidebar")));
        let mut app = Self {
            doc: Document::default(),
            code: CodeView::with_fonts(fonts.clone(), egui::Id::new("code_view")),
            live: LiveView::with_fonts(fonts, egui::Id::new("live_view")),
            mode: Mode::Split,
            focus: Pane::Code,
            parse: {
                let ctx = ctx.clone();
                // GitHub Flavored Markdown; PulldownParser is plain CommonMark.
                ParseState::new(Arc::new(GfmParser), &Document::default(), move || {
                    ctx.request_repaint()
                })
            },
            banner: None,
            error: None,
            hint: None,
            back: Vec::new(),
            pending_jump: None,
            #[cfg(test)]
            opened_urls: Vec::new(),
            dialog: None,
            confirm: None,
            close_after_save: false,
            close_allowed: false,
            next_disk_check: Instant::now() + DISK_CHECK_INTERVAL,
            title: String::new(),
            measure: None,
            recent,
            recent_list: None,
            browser: FileBrowser::new(root),
            sidebar: sidebar::Sidebar::load(sidebar_store),
            new_file: None,
            rename: None,
            trash_confirm: None,
            trash: Box::new(SystemTrash),
            modal_was_open: false,
            #[cfg(test)]
            dialog_hook: None,
        };
        // A file (including one that doesn't exist yet) browses its parent.
        // A folder browses that folder with nothing open. No argument browses
        // the current directory and still offers recent files.
        match launch {
            Launch::File { file, .. } => app.open(file),
            Launch::Folder { .. } => {
                if path.is_none() && !app.recent.entries().is_empty() {
                    app.recent_list = Some(0);
                }
            }
        }
        app.code.request_focus(ctx);
        app
    }

    /// Opens `path`, asking first if there are unsaved changes.
    fn request_open(&mut self, path: PathBuf) {
        if self.doc.is_dirty() {
            self.confirm = Some(Confirm::OpenPath(path));
        } else {
            self.open(path);
        }
    }

    fn open(&mut self, path: PathBuf) {
        let doc = match Document::open(&path) {
            Ok(doc) => doc,
            // A path that doesn't exist yet becomes a new file there.
            Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut doc = Document::default();
                doc.set_path(&path);
                doc
            }
            Err(e) => {
                self.error = Some(format!("Couldn't open {}: {e}", path.display()));
                return;
            }
        };
        // Only files that opened (or are deliberately new) are remembered.
        self.recent.add(&path);
        self.replace_document(doc);
    }

    fn replace_document(&mut self, doc: Document) {
        self.doc = doc;
        self.code.reset();
        self.live.reset();
        self.parse.reset(&self.doc);
        self.banner = None;
        self.error = None;
    }

    /// Re-reads the open file. If it's gone, the buffer is kept (it may be
    /// the only copy left) and the missing-file banner says so.
    fn reload(&mut self) {
        let Some(path) = self.doc.path().map(|p| p.to_path_buf()) else {
            return;
        };
        match Document::open(&path) {
            Ok(doc) => self.replace_document(doc),
            Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.banner = Some(Banner::DiskMissing);
            }
            Err(e) => {
                self.error = Some(format!("Couldn't reload {}: {e}", path.display()));
            }
        }
    }

    fn save(&mut self) {
        if self.doc.path().is_none() {
            return self.spawn_dialog(DialogKind::SaveAs);
        }
        // Never overwrite someone else's changes without asking.
        if matches!(self.doc.disk_status(), Ok(DiskStatus::Modified)) {
            self.banner = Some(Banner::DiskChanged);
            return;
        }
        match self.doc.save() {
            Ok(()) => self.saved(),
            Err(e) => self.error = Some(format!("Couldn't save: {e}")),
        }
    }

    fn save_as(&mut self, path: PathBuf) {
        // Saving as the open file is a save: don't overwrite changes made
        // by another program without asking.
        let canonical = |p: &std::path::Path| std::fs::canonicalize(p).ok();
        let same_file = self.doc.path().is_some_and(|open| {
            open == path || canonical(open).is_some_and(|c| Some(c) == canonical(&path))
        });
        if same_file && matches!(self.doc.disk_status(), Ok(DiskStatus::Modified)) {
            self.banner = Some(Banner::DiskChanged);
            return;
        }
        match self.doc.save_as(&path) {
            Ok(()) => {
                self.recent.add(&path);
                self.saved();
            }
            Err(e) => self.error = Some(format!("Couldn't save {}: {e}", path.display())),
        }
    }

    fn saved(&mut self) {
        self.banner = None;
        self.error = None;
        if self.close_after_save {
            self.close_allowed = true;
        }
    }

    /// Runs a portal dialog on a thread so the UI keeps drawing. Open, Save
    /// As and Open Folder share one channel; a second dialog waits.
    fn spawn_dialog(&mut self, kind: DialogKind) {
        #[cfg(test)]
        if let Some(hook) = &self.dialog_hook {
            let _ = hook.send(kind.clone());
            return;
        }
        if self.dialog.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let dir = self
            .doc
            .path()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .or_else(|| Some(self.browser.root().to_path_buf()));
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(dir) = &dir {
                dialog = dialog.set_directory(dir);
            }
            let result = match kind {
                DialogKind::Open => DialogResult::Open(
                    dialog
                        .add_filter("Markdown", MARKDOWN_EXTENSIONS)
                        .pick_file(),
                ),
                DialogKind::SaveAs => DialogResult::SaveAs(
                    dialog
                        .add_filter("Markdown", MARKDOWN_EXTENSIONS)
                        .set_file_name("untitled.md")
                        .save_file(),
                ),
                DialogKind::Folder => DialogResult::Folder(dialog.pick_folder()),
                DialogKind::MoveTo(path) => {
                    let folder = dialog.set_title("Move to…").pick_folder();
                    DialogResult::MoveTo(path, folder)
                }
            };
            let _ = tx.send(result);
        });
        self.dialog = Some(rx);
    }

    fn poll_dialog(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.dialog else { return };
        match rx.try_recv() {
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
            // Edits may have been made while the dialog was up.
            Ok(DialogResult::Open(Some(path))) => self.request_open(path),
            Ok(DialogResult::SaveAs(Some(path))) => self.save_as(path),
            Ok(DialogResult::Folder(Some(path))) => {
                self.browser.set_root(path);
                // The picker can be opened while the sidebar is hidden.
                self.sidebar.visible = true;
                self.sidebar.save();
                self.browser.request_focus();
            }
            Ok(DialogResult::Open(None) | DialogResult::SaveAs(None)) => {
                self.close_after_save = false;
            }
            Ok(DialogResult::MoveTo(path, Some(dir))) => self.move_entry(path, dir),
            Ok(DialogResult::Folder(None) | DialogResult::MoveTo(_, None)) => {}
            Err(TryRecvError::Disconnected) => {
                self.error = Some("The file dialog failed. Is xdg-desktop-portal running?".into());
            }
        }
        self.dialog = None;
    }

    fn check_disk(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if now >= self.next_disk_check {
            self.next_disk_check = now + DISK_CHECK_INTERVAL;
            self.banner = match self.doc.disk_status() {
                Ok(DiskStatus::Modified) => Some(Banner::DiskChanged),
                Ok(DiskStatus::Missing) => Some(Banner::DiskMissing),
                _ => None,
            };
        }
        ctx.request_repaint_after(DISK_CHECK_INTERVAL);
    }

    fn selection(&self) -> inkmark_buffer::Selection {
        match self.focus {
            Pane::Code => self.code.selection(),
            Pane::Live => self.live.selection(),
        }
    }

    /// Moves keyboard focus (with the caret and scroll position) to `pane`,
    /// switching away from a single-pane mode that hides it.
    fn focus_pane(&mut self, ctx: &egui::Context, pane: Pane) {
        let selection = self.selection();
        let parse = self.parse.output();
        match pane {
            Pane::Code => {
                if self.mode == Mode::Live {
                    self.code
                        .set_scroll_pos(self.live.scroll_pos(&self.doc, parse));
                    self.mode = Mode::Code;
                }
                self.code.set_selection(selection);
                self.code.request_focus(ctx);
            }
            Pane::Live => {
                if self.mode == Mode::Code {
                    self.live
                        .set_scroll_pos(&self.doc, parse, self.code.scroll_pos());
                    self.mode = Mode::Live;
                }
                self.live.set_selection(selection);
                self.live.request_focus(ctx);
            }
        }
        self.focus = pane;
    }

    /// Split → code → live → split.
    fn cycle_mode(&mut self, ctx: &egui::Context) {
        match self.mode {
            Mode::Split => {
                // Pretend live was showing so focus_pane carries its position.
                self.mode = Mode::Live;
                self.focus_pane(ctx, Pane::Code);
            }
            Mode::Code => self.focus_pane(ctx, Pane::Live),
            Mode::Live => {
                // The code pane was hidden: bring it to where live is.
                let pos = self.live.scroll_pos(&self.doc, self.parse.output());
                self.code.set_scroll_pos(pos);
                self.mode = Mode::Split;
                self.focus_pane(ctx, Pane::Live);
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (browser, cycle, code, live, minimap, recent) = ctx.input_mut(|i| {
            // egui ignores extra Shift and Alt when matching, so the shifted
            // browser shortcut has to be consumed before Ctrl+E.
            // Ctrl+Alt+digit sets headings.
            let alt = i.modifiers.alt;
            (
                i.consume_shortcut(&TOGGLE_BROWSER),
                i.consume_shortcut(&CYCLE_MODE),
                !alt && i.consume_shortcut(&FOCUS_CODE),
                !alt && i.consume_shortcut(&FOCUS_LIVE),
                i.consume_shortcut(&TOGGLE_MINIMAP),
                i.consume_shortcut(&RECENT),
            )
        });
        if browser {
            self.sidebar.visible = !self.sidebar.visible;
            self.sidebar.save();
            if self.sidebar.visible {
                self.browser.request_focus();
            } else {
                self.focus_pane(ctx, self.focus);
            }
        }
        if recent {
            self.recent_list = match self.recent_list {
                Some(_) => None,
                None => {
                    self.new_file = None;
                    Some(0)
                }
            };
        }
        if minimap {
            // Each pane keeps its own minimap setting.
            match self.focus {
                Pane::Code => self.code.show_minimap ^= true,
                Pane::Live => self.live.show_minimap ^= true,
            }
        }
        if cycle {
            self.cycle_mode(ctx);
        } else if code {
            self.focus_pane(ctx, Pane::Code);
        } else if live {
            self.focus_pane(ctx, Pane::Live);
        }
        let (open_folder, save_as, save, open, new_file, back) = ctx.input_mut(|i| {
            (
                // Ctrl+Shift+O before Ctrl+O: extra Shift still matches Open.
                i.consume_shortcut(&OPEN_FOLDER),
                i.consume_shortcut(&SAVE_AS),
                i.consume_shortcut(&SAVE),
                i.consume_shortcut(&OPEN),
                i.consume_shortcut(&NEW_FILE),
                i.consume_shortcut(&BACK),
            )
        });
        if back {
            self.go_back();
        }
        if open_folder {
            self.spawn_dialog(DialogKind::Folder);
        } else if save_as {
            self.spawn_dialog(DialogKind::SaveAs);
        } else if save {
            self.save();
        }
        if open {
            if self.doc.is_dirty() {
                self.confirm = Some(Confirm::Open);
            } else {
                self.spawn_dialog(DialogKind::Open);
            }
        }
        if new_file {
            self.begin_new_file();
        }
    }

    fn guard_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested())
            && self.doc.is_dirty()
            && !self.close_allowed
        {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Close);
        }
        if self.close_allowed {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn file_name(&self) -> String {
        self.doc
            .path()
            .and_then(|p| p.file_name())
            .map_or_else(|| "untitled".into(), |n| n.to_string_lossy().into_owned())
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let dirty = if self.doc.is_dirty() { "● " } else { "" };
        let title = format!("{dirty}{} — inkmark", self.file_name());
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn banner_ui(&mut self, ui: &mut egui::Ui) {
        let mut action = None;
        if let Some(message) = &self.error {
            ui.horizontal(|ui| {
                ui.label(RichText::new(message).color(Color32::from_rgb(255, 140, 120)));
                if ui.button("Dismiss").clicked() {
                    action = Some("dismiss");
                }
            });
        }
        let Some(banner) = &self.banner else {
            if action == Some("dismiss") {
                self.error = None;
            }
            return;
        };
        ui.horizontal(|ui| match banner {
            Banner::DiskChanged => {
                ui.label("This file was changed by another program.");
                if ui.button("Reload").clicked() {
                    action = Some("reload");
                }
                if ui.button("Keep mine").clicked() {
                    action = Some("keep");
                }
            }
            Banner::DiskMissing => {
                ui.label("This file was deleted or moved. Saving will recreate it.");
                if ui.button("Dismiss").clicked() {
                    action = Some("keep");
                }
            }
        });
        match action {
            Some("dismiss") => self.error = None,
            Some("reload") => self.reload(),
            Some("keep") => {
                self.banner = None;
                if let Err(e) = self.doc.acknowledge_disk_state() {
                    self.error = Some(format!("Couldn't check the file: {e}"));
                }
            }
            _ => {}
        }
    }

    fn status_ui(&self, ui: &mut egui::Ui) {
        let head = self.selection().head;
        let line = self.doc.byte_to_line(head);
        let column = self
            .doc
            .slice(self.doc.line_to_byte(line)..head)
            .chars()
            .count()
            + 1;
        let encoding = self.doc.encoding();
        ui.horizontal(|ui| {
            let path = self
                .doc
                .path()
                .map_or_else(|| "untitled".into(), |p| p.display().to_string());
            ui.label(path);
            if self.doc.is_dirty() {
                ui.label("●");
            }
            if let Some((hint, _)) = &self.hint {
                ui.label(RichText::new(hint).color(Color32::from_rgb(230, 200, 120)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(if encoding.bom { "UTF-8 BOM" } else { "UTF-8" });
                ui.label(match encoding.line_ending {
                    LineEnding::Lf => "LF",
                    LineEnding::CrLf => "CRLF",
                });
                ui.label(format!("Ln {}, Col {column}", line + 1));
            });
        });
    }

    /// While a dialog is open the panes don't get keys (typing mustn't edit
    /// the document behind it); focus returns when it closes.
    fn hold_focus_for_dialogs(&mut self, ctx: &egui::Context) {
        let open = self.recent_list.is_some()
            || self.confirm.is_some()
            || self.new_file.is_some()
            || self.rename.is_some()
            || self.trash_confirm.is_some();
        if open {
            self.code.release_focus(ctx);
            self.live.release_focus(ctx);
            ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("file_browser")));
        } else if self.modal_was_open {
            match self.focus {
                Pane::Code => self.code.request_focus(ctx),
                Pane::Live => self.live.request_focus(ctx),
            }
        }
        self.modal_was_open = open;
    }

    /// The recent-files list: arrows and Enter, or a click, open one.
    fn recent_ui(&mut self, ctx: &egui::Context) {
        let Some(mut selected) = self.recent_list else {
            return;
        };
        let entries = self.recent.entries().to_vec();
        let (up, down, enter, escape) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if entries.is_empty() || escape {
            self.recent_list = None;
            return;
        }
        if up {
            selected = selected.saturating_sub(1);
        }
        if down {
            selected = (selected + 1).min(entries.len() - 1);
        }
        let mut chosen = enter.then(|| entries[selected].clone());
        egui::Modal::new(egui::Id::new("recent")).show(ctx, |ui| {
            ui.set_min_width(480.0);
            ui.heading("Recent files");
            ui.add_space(6.0);
            for (i, path) in entries.iter().enumerate() {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                let dir = path
                    .parent()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default();
                let missing = if path.exists() { "" } else { "  (missing)" };
                let text = RichText::new(format!("{name}{missing}")).strong();
                let row = ui.selectable_label(i == selected, text).on_hover_text(&dir);
                ui.label(RichText::new(dir).small().weak());
                if row.clicked() {
                    chosen = Some(path.clone());
                }
            }
            ui.add_space(6.0);
            ui.label(
                RichText::new("Up/Down select · Enter open · Esc close")
                    .small()
                    .weak(),
            );
        });
        self.recent_list = Some(selected);
        if let Some(path) = chosen {
            self.recent_list = None;
            self.request_open(path);
        }
    }

    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm.clone() else {
            return;
        };
        let mut choice = None;
        egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            ui.label(format!("{} has unsaved changes.", self.file_name()));
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some("save");
                }
                if ui.button("Discard").clicked() {
                    choice = Some("discard");
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    choice = Some("cancel");
                }
            });
        });
        let Some(choice) = choice else { return };
        self.confirm = None;
        match (choice, confirm) {
            ("save", Confirm::Close) => {
                self.close_after_save = true;
                self.save();
            }
            ("save", Confirm::Open) => {
                self.save();
                if !self.doc.is_dirty() {
                    self.spawn_dialog(DialogKind::Open);
                }
            }
            ("save", Confirm::OpenPath(path)) => {
                self.save();
                if !self.doc.is_dirty() {
                    self.open(path);
                }
            }
            ("discard", Confirm::OpenPath(path)) => self.open(path),
            ("discard", Confirm::Close) => self.close_allowed = true,
            ("discard", Confirm::Open) => self.spawn_dialog(DialogKind::Open),
            _ => {}
        }
    }

    fn begin_new_file(&mut self) {
        if self.new_file.is_some() || self.confirm.is_some() {
            return;
        }
        self.recent_list = None;
        self.new_file = Some(NewFilePrompt {
            dir: self.browser.new_file_dir(),
            name: String::new(),
            error: None,
        });
    }

    /// Creates the named file and opens it through the unsaved-changes prompt.
    /// Cancelling that prompt leaves the empty file on disk.
    fn submit_new_file(&mut self) {
        let Some(prompt) = self.new_file.clone() else {
            return;
        };
        match create_new_file(&prompt.dir, &prompt.name) {
            Ok(path) => {
                self.new_file = None;
                self.browser.note_created(&path);
                self.request_open(path);
            }
            Err(NewFileError::Exists(_)) => {
                if let Some(prompt) = &mut self.new_file {
                    prompt.error = Some("A file with that name already exists.".into());
                }
            }
            Err(NewFileError::Empty | NewFileError::Invalid) => {
                if let Some(prompt) = &mut self.new_file {
                    prompt.error = Some("Enter a file name.".into());
                }
            }
            Err(NewFileError::Io(message)) => {
                self.new_file = None;
                self.error = Some(format!("Couldn't create the file: {message}"));
            }
        }
    }

    fn new_file_ui(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.new_file.clone() else {
            return;
        };
        let detail = prompt.dir.display().to_string();
        let (submit, cancel) = name_modal(
            ctx,
            "new_file",
            "New file",
            &detail,
            "Create",
            &mut prompt.name,
            prompt.error.as_deref(),
        );
        if cancel {
            self.new_file = None;
            return;
        }
        self.new_file = Some(prompt);
        if submit {
            self.submit_new_file();
        }
    }

    /// Rename… for a sidebar entry. A Markdown file's name is offered
    /// without its extension, which `rename` keeps.
    fn begin_rename(&mut self, path: PathBuf) {
        if self.confirm.is_some() || self.new_file.is_some() {
            return;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let name = if !path.is_dir() && inkmark_files::is_markdown_name(&name) {
            path.file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            name.into_owned()
        };
        self.recent_list = None;
        self.rename = Some(RenamePrompt {
            path,
            name,
            error: None,
        });
    }

    fn rename_ui(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.rename.clone() else {
            return;
        };
        let detail = prompt.path.display().to_string();
        let (submit, cancel) = name_modal(
            ctx,
            "rename",
            "Rename",
            &detail,
            "Rename",
            &mut prompt.name,
            prompt.error.as_deref(),
        );
        if cancel {
            self.rename = None;
            return;
        }
        self.rename = Some(prompt.clone());
        if !submit {
            return;
        }
        let was_unchanged = self.unchanged_if_affected(&prompt.path);
        match rename(&prompt.path, &prompt.name) {
            Ok(new) => {
                self.rename = None;
                self.moved(&prompt.path, &new, was_unchanged);
            }
            Err(e @ (inkmark_files::OpError::Exists(_) | inkmark_files::OpError::Invalid)) => {
                if let Some(prompt) = &mut self.rename {
                    // Not capitalized: the message may start with a file name.
                    prompt.error = Some(format!("Can't rename: {e}"));
                }
            }
            Err(e) => {
                self.rename = None;
                self.error = Some(format!(
                    "Couldn't rename {}: {e}",
                    display_name(&prompt.path)
                ));
            }
        }
    }

    fn move_entry(&mut self, path: PathBuf, dir: PathBuf) {
        let was_unchanged = self.unchanged_if_affected(&path);
        match move_into(&path, &dir) {
            Ok(new) if new == path => {}
            Ok(new) => self.moved(&path, &new, was_unchanged),
            Err(e) => {
                self.error = Some(format!("Couldn't move {}: {e}", display_name(&path)));
            }
        }
    }

    /// Whether the open document is `path` or inside it, and matched the
    /// disk just now (asked before a rename or move changes its ctime).
    /// `false` when the document isn't affected.
    fn unchanged_if_affected(&self, path: &Path) -> bool {
        self.doc.path().is_some_and(|open| open.starts_with(path))
            && matches!(self.doc.disk_status(), Ok(DiskStatus::Unchanged))
    }

    /// `old` became `new`: the document, the recent list and the sidebar
    /// follow.
    fn moved(&mut self, old: &Path, new: &Path, was_unchanged: bool) {
        if let Some(open) = self.doc.path().map(Path::to_path_buf)
            && let Ok(rest) = open.strip_prefix(old)
        {
            let followed = if rest.as_os_str().is_empty() {
                new.to_path_buf()
            } else {
                new.join(rest)
            };
            self.doc.moved_to(followed, was_unchanged);
        }
        self.recent.moved(old, new);
        for dir in [old.parent(), new.parent()].into_iter().flatten() {
            self.browser.refresh_dir(dir);
        }
        self.browser.select(new);
    }

    fn trash_ui(&mut self, ctx: &egui::Context) {
        let Some(path) = self.trash_confirm.clone() else {
            return;
        };
        let (confirm_key, cancel_key) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        let (mut confirm, mut cancel) = (confirm_key, cancel_key);
        let open_inside = self.doc.path().is_some_and(|open| open.starts_with(&path));
        egui::Modal::new(egui::Id::new("trash")).show(ctx, |ui| {
            ui.set_min_width(420.0);
            ui.heading(format!("Move “{}” to the trash?", display_name(&path)));
            ui.label("You can restore it from your file manager's trash.");
            if open_inside {
                ui.label("It's open: your text stays in the editor until you close it.");
            }
            ui.horizontal(|ui| {
                if ui.button("Move to Trash").clicked() {
                    confirm = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if cancel {
            self.trash_confirm = None;
        } else if confirm {
            self.trash_confirm = None;
            match self.trash.trash(&path) {
                Ok(()) => {
                    if let Some(dir) = path.parent() {
                        self.browser.refresh_dir(dir);
                    }
                    if open_inside {
                        self.banner = Some(Banner::DiskMissing);
                    }
                }
                Err(e) => {
                    self.error = Some(format!(
                        "Couldn't move {} to the trash: {e}",
                        display_name(&path)
                    ));
                }
            }
        }
    }
}

impl App {
    /// Sidebar beside the panes. Hidden, the panes keep the whole width.
    fn editor(&mut self, ui: &mut egui::Ui) {
        self.browser
            .set_current(self.doc.path().map(|path| path.to_path_buf()));
        if !self.sidebar.visible {
            self.browser.poll_listings(ui.ctx());
            self.panes(ui);
            return;
        }
        let rect = ui.available_rect_before_wrap();
        let width = self
            .sidebar
            .width
            .clamp(sidebar::MIN_WIDTH, sidebar::MAX_WIDTH)
            .min((rect.width() - 240.0).max(sidebar::MIN_WIDTH));
        let gap = 4.0;
        let left = Rect::from_min_max(rect.min, pos2(rect.left() + width, rect.bottom()));
        let handle = Rect::from_min_max(
            pos2(left.right(), rect.top()),
            pos2(left.right() + gap, rect.bottom()),
        );
        let right = Rect::from_min_max(pos2(handle.right(), rect.top()), rect.max);
        let mut output = BrowserOutput::default();
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            output = self.browser.show(ui);
        });
        self.apply_browser(&output);
        let handle_resp = ui.interact(handle, egui::Id::new("sidebar_split"), egui::Sense::drag());
        if handle_resp.hovered() || handle_resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
        if handle_resp.dragged() {
            self.sidebar.width = (self.sidebar.width + handle_resp.drag_delta().x)
                .clamp(sidebar::MIN_WIDTH, sidebar::MAX_WIDTH);
        }
        // The pointer can move for many frames. Store the width once, on release.
        if handle_resp.drag_stopped() {
            self.sidebar.save();
        }
        ui.painter().vline(
            handle.center().x,
            handle.y_range(),
            Stroke::new(1.0, Color32::from_gray(40)),
        );
        if right.width() > 1.0 {
            ui.scope_builder(UiBuilder::new().max_rect(right), |ui| self.panes(ui));
        }
    }

    fn apply_browser(&mut self, output: &BrowserOutput) {
        if output.open_folder {
            self.spawn_dialog(DialogKind::Folder);
        }
        if let Some(path) = &output.open_file {
            self.request_open(path.clone());
        }
        if output.new_file {
            self.begin_new_file();
        }
        if let Some(path) = &output.rename {
            self.begin_rename(path.clone());
        }
        if let Some(path) = &output.trash
            && self.confirm.is_none()
        {
            self.trash_confirm = Some(path.clone());
        }
        if let Some(path) = &output.move_to {
            self.spawn_dialog(DialogKind::MoveTo(path.clone()));
        }
        if let Some((path, dir)) = &output.dropped {
            self.move_entry(path.clone(), dir.clone());
        }
    }

    fn show_hint(&mut self, hint: impl Into<String>) {
        self.hint = Some((hint.into(), Instant::now()));
    }

    /// Follows the link at `at` in the open document (Ctrl+click).
    fn follow(&mut self, at: usize) {
        let Some(link) = inkmark_parse::link_at(&self.doc, self.parse.output(), at) else {
            return;
        };
        let here = Place {
            path: self.doc.path().map(Path::to_path_buf),
            offset: self.selection().head,
        };
        let dest = match link {
            inkmark_parse::Link::Footnote(label) => {
                match inkmark_parse::footnote_offset(&self.doc, self.parse.output(), &label) {
                    Some(offset) => {
                        self.back.push(here);
                        self.jump_to(offset);
                    }
                    None => self.show_hint(format!("No note [^{label}] in this file")),
                }
                return;
            }
            inkmark_parse::Link::Dest(dest) => dest,
        };
        let base = self
            .doc
            .path()
            .and_then(Path::parent)
            .map_or_else(|| self.browser.root().to_path_buf(), Path::to_path_buf);
        match links::resolve(&dest, &base) {
            links::Target::External(url) => self.open_external(url),
            links::Target::Anchor(anchor) => {
                if self.jump_to_anchor(&anchor) {
                    self.back.push(here);
                }
            }
            links::Target::File { path, anchor } if Some(path.as_path()) == self.doc.path() => {
                if anchor.is_none_or(|a| self.jump_to_anchor(&a)) {
                    self.back.push(here);
                }
            }
            links::Target::File { path, anchor } => {
                self.back.push(here);
                self.pending_jump = anchor.map(|a| (path.clone(), Jump::Anchor(a)));
                self.request_open(path);
            }
            links::Target::Refused(reason) => self.show_hint(reason),
        }
    }

    /// Alt+Left: back to where the last followed link was clicked.
    fn go_back(&mut self) {
        let Some(place) = self.back.pop() else {
            return;
        };
        match place.path {
            Some(path) if Some(path.as_path()) != self.doc.path() => {
                self.pending_jump = Some((path.clone(), Jump::Offset(place.offset)));
                self.request_open(path);
            }
            _ => self.jump_to(place.offset.min(self.doc.len())),
        }
    }

    fn jump_to_anchor(&mut self, anchor: &str) -> bool {
        match inkmark_parse::heading_offset(&self.doc, self.parse.output(), anchor) {
            Some(offset) => {
                self.jump_to(offset);
                true
            }
            None => {
                self.show_hint(format!("No heading #{anchor} in this file"));
                false
            }
        }
    }

    /// Puts the caret at `offset` in the focused pane, scrolled into view.
    fn jump_to(&mut self, offset: usize) {
        let caret = inkmark_buffer::Selection::caret(offset);
        match self.focus {
            Pane::Code => self.code.set_selection(caret),
            Pane::Live => self.live.set_selection(caret),
        }
    }

    /// Applies a jump waiting on a file opened from a link, once that file
    /// is open and parsed. Dropped if another file ended up open instead
    /// (the open failed, or the unsaved-changes prompt was cancelled).
    fn apply_pending_jump(&mut self) {
        let Some((path, jump)) = self.pending_jump.clone() else {
            return;
        };
        if self.doc.path() != Some(path.as_path()) {
            if self.confirm.is_none() {
                self.pending_jump = None;
            }
            return;
        }
        if !self.parse.is_settled() {
            return;
        }
        self.pending_jump = None;
        match jump {
            Jump::Anchor(anchor) => {
                self.jump_to_anchor(&anchor);
            }
            Jump::Offset(offset) => self.jump_to(offset.min(self.doc.len())),
        }
    }

    /// Opens an `http(s)` or `mailto` link in the default app.
    fn open_external(&mut self, url: String) {
        #[cfg(test)]
        {
            self.opened_urls.push(url);
        }
        #[cfg(not(test))]
        match std::process::Command::new("xdg-open").arg(&url).spawn() {
            // Reaped on a thread so it doesn't linger as a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(e) => self.error = Some(format!("Couldn't open {url}: {e}")),
        }
    }

    fn panes(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if let Some(hint) = self.live.take_hint() {
            self.show_hint(hint);
        }
        // Ctrl+clicks from the last frame, now that the click is over.
        if let Some(at) = self.live.take_follow().or(self.code.take_follow()) {
            self.follow(at);
        }
        self.apply_pending_jump();
        if let Some((_, shown)) = &self.hint {
            let shown = *shown;
            if shown.elapsed() >= HINT_TIME {
                self.hint = None;
            } else {
                ctx.request_repaint_after(HINT_TIME - shown.elapsed());
            }
        }
        match self.mode {
            Mode::Code => {
                self.code.show(ui, &mut self.doc, Some(&mut self.parse));
            }
            Mode::Live => {
                self.live.show(ui, &mut self.doc, Some(&mut self.parse));
            }
            Mode::Split => {
                let rect = ui.available_rect_before_wrap();
                let mid = rect.center().x.round();
                let left = Rect::from_min_max(rect.min, pos2(mid - 1.0, rect.bottom()));
                let right = Rect::from_min_max(pos2(mid + 1.0, rect.top()), rect.max);
                ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
                    self.code.show(ui, &mut self.doc, Some(&mut self.parse));
                });
                ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
                    self.live.show(ui, &mut self.doc, Some(&mut self.parse));
                });
                ui.painter().vline(
                    mid,
                    rect.y_range(),
                    Stroke::new(2.0, Color32::from_gray(40)),
                );
            }
        }

        // Clicking into a pane moves focus there too.
        if self.code.has_focus(&ctx) {
            self.focus = Pane::Code;
        } else if self.live.has_focus(&ctx) {
            self.focus = Pane::Live;
        }
        // The other pane mirrors the caret and follows the scroll position.
        let (code_scrolled, live_scrolled) = (self.code.take_scrolled(), self.live.take_scrolled());
        let parse = self.parse.output();
        match self.focus {
            Pane::Code => self.live.mirror_selection(self.code.selection()),
            Pane::Live => self.code.mirror_selection(self.live.selection()),
        }
        if self.mode == Mode::Split && parse.map.len() == self.doc.len() {
            if code_scrolled {
                self.live
                    .set_scroll_pos(&self.doc, parse, self.code.scroll_pos());
                ctx.request_repaint();
            } else if live_scrolled {
                self.code
                    .set_scroll_pos(self.live.scroll_pos(&self.doc, parse));
                ctx.request_repaint();
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_shortcuts(&ctx);
        self.poll_dialog(&ctx);
        self.check_disk(&ctx);
        self.guard_close(&ctx);
        // Dialogs take the keyboard before the panes see it.
        self.recent_ui(&ctx);
        self.hold_focus_for_dialogs(&ctx);

        if self.banner.is_some() || self.error.is_some() {
            egui::Panel::top("banner").show(ui, |ui| self.banner_ui(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_ui(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.editor(ui));
        // After the sidebar, so New file opens the prompt on the click's frame.
        self.new_file_ui(&ctx);
        self.rename_ui(&ctx);
        self.trash_ui(&ctx);
        self.confirm_ui(&ctx);
        self.update_title(&ctx);
        self.measure_step(&ctx);
    }
}

impl App {
    fn measure_step(&mut self, ctx: &egui::Context) {
        let Some(m) = &mut self.measure else { return };
        match m.frame(ctx, self.parse.is_settled(), self.doc.line_count()) {
            measure::Step::Idle => {}
            measure::Step::ScrollTo(line) => {
                let pos = inkmark_view::ScrollPos { line, frac: 0.0 };
                self.code.set_scroll_pos(pos);
                self.live
                    .set_scroll_pos(&self.doc, self.parse.output(), pos);
            }
            measure::Step::Quit => {
                self.close_allowed = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }
}

/// A file or folder's name for messages.
fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// A small dialog asking for a name. Returns (submit, cancel); Enter
/// submits and Escape cancels.
fn name_modal(
    ctx: &egui::Context,
    id: &str,
    heading: &str,
    detail: &str,
    action: &str,
    name: &mut String,
    error: Option<&str>,
) -> (bool, bool) {
    let (mut submit, mut cancel) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::Enter),
            i.consume_key(Modifiers::NONE, Key::Escape),
        )
    });
    let field = egui::Id::new((id, "name"));
    egui::Modal::new(egui::Id::new(id)).show(ctx, |ui| {
        ui.set_min_width(420.0);
        ui.heading(heading);
        ui.label(detail);
        if !ui.memory(|m| m.has_focus(field)) {
            ui.memory_mut(|m| m.request_focus(field));
        }
        ui.add(
            egui::TextEdit::singleline(name)
                .id(field)
                .desired_width(f32::INFINITY),
        );
        if let Some(error) = error {
            ui.label(RichText::new(error).color(Color32::from_rgb(255, 140, 120)));
        }
        ui.horizontal(|ui| {
            if ui.button(action).clicked() {
                submit = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    (submit, cancel)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// An app with its recent-files list kept in `dir`, never the real one.
    fn app(dir: &std::path::Path, path: Option<PathBuf>) -> App {
        let ctx = egui::Context::default();
        let recent = recent::Recent::from_store(Some(dir.join("recent")));
        App::with_recent(&ctx, path, recent)
    }

    fn text(app: &App) -> String {
        app.doc.slice(0..app.doc.len()).into_owned()
    }

    fn type_into(app: &mut App, s: &str) {
        let end = app.doc.len();
        app.doc
            .apply(
                vec![inkmark_buffer::Edit::insert(end, s)],
                inkmark_buffer::Selection::caret(end),
                inkmark_buffer::Selection::caret(end + s.len()),
                inkmark_buffer::EditKind::Other,
            )
            .unwrap();
    }

    #[test]
    fn a_failed_open_is_not_remembered() {
        // Regression for #3.
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.md");
        fs::write(&bad, b"\xff\xfe not utf-8").unwrap();
        let mut app = app(dir.path(), None);
        app.open(bad.clone());
        assert!(app.error.is_some());
        assert!(app.recent.entries().is_empty());
        // A new file at a missing path is deliberate, so it is remembered.
        app.open(dir.path().join("new.md"));
        assert_eq!(app.recent.entries().len(), 1);
    }

    #[test]
    fn reloading_a_deleted_file_keeps_the_buffer() {
        // Regression for #5.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# notes\n").unwrap();
        let mut app = app(dir.path(), Some(path.clone()));
        type_into(&mut app, "unsaved\n");
        fs::remove_file(&path).unwrap();
        app.reload();
        assert_eq!(text(&app), "# notes\nunsaved\n");
        assert!(app.doc.is_dirty());
        assert!(matches!(app.banner, Some(Banner::DiskMissing)));
    }

    #[test]
    fn save_as_the_open_file_respects_changes_on_disk() {
        // Regression for #6.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "one\n").unwrap();
        let mut app = app(dir.path(), Some(path.clone()));
        type_into(&mut app, "mine\n");
        fs::write(&path, "someone else's longer text\n").unwrap();
        app.save_as(path.clone());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "someone else's longer text\n"
        );
        assert!(matches!(app.banner, Some(Banner::DiskChanged)));
        // Saving somewhere else is fine.
        app.save_as(dir.path().join("copy.md"));
        assert_eq!(
            fs::read_to_string(dir.path().join("copy.md")).unwrap(),
            "one\nmine\n"
        );
    }

    #[test]
    fn a_file_chosen_in_the_open_dialog_asks_before_dropping_edits() {
        // Regression for #7.
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other.md");
        fs::write(&other, "other\n").unwrap();
        let mut app = app(dir.path(), None);
        let (tx, rx) = mpsc::channel();
        app.dialog = Some(rx);
        // Typing while the dialog is open.
        type_into(&mut app, "draft");
        tx.send(DialogResult::Open(Some(other.clone()))).unwrap();
        app.poll_dialog(&egui::Context::default());
        assert_eq!(text(&app), "draft");
        assert_eq!(app.confirm, Some(Confirm::OpenPath(other)));
    }

    #[test]
    fn command_line_picks_the_browser_root() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("notes");
        fs::create_dir(&folder).unwrap();
        let file = folder.join("a.md");
        fs::write(&file, "# a\n").unwrap();
        let missing = folder.join("new.md");

        let opened = app(dir.path(), Some(file.clone()));
        assert_eq!(opened.browser.root(), folder);
        assert_eq!(opened.doc.path(), Some(file.as_path()));

        let browsed = app(dir.path(), Some(folder.clone()));
        assert_eq!(browsed.browser.root(), folder);
        assert!(browsed.doc.path().is_none());
        assert!(browsed.recent_list.is_none());
        // Browsing a folder does not remember the folder as a file. The
        // shared test store may still hold the file opened above.
        assert!(browsed.recent.entries().iter().all(|p| p != &folder));

        let created = app(dir.path(), Some(missing.clone()));
        assert_eq!(created.browser.root(), folder);
        assert_eq!(created.doc.path(), Some(missing.as_path()));
        assert_eq!(text(&created), "");

        let cwd = std::env::current_dir().unwrap();
        let ctx = egui::Context::default();
        let here = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("empty-recent"))),
        );
        assert_eq!(here.browser.root(), cwd);
        assert!(here.doc.path().is_none());
        assert!(here.recent_list.is_none());

        // Recent files are still offered when nothing was passed, and not
        // when a folder was.
        let remembered = dir.path().join("old.md");
        fs::write(&remembered, "old\n").unwrap();
        let store = dir.path().join("recent2");
        let mut recent = recent::Recent::from_store(Some(store.clone()));
        recent.add(&remembered);
        let offered = App::with_recent(&ctx, None, recent);
        assert_eq!(offered.recent_list, Some(0));
        assert_eq!(offered.browser.root(), cwd);
        let again = recent::Recent::from_store(Some(store));
        let folder_hides_recent = App::with_recent(&ctx, Some(folder), again);
        assert!(folder_hides_recent.recent_list.is_none());
        assert_eq!(folder_hides_recent.browser.root(), dir.path().join("notes"));
    }

    #[test]
    fn leaving_live_only_mode_brings_the_code_pane_along() {
        // Regression for #2.
        let dir = tempfile::tempdir().unwrap();
        let text: String = (0..200).map(|i| format!("line {i}\n\n")).collect();
        let path = dir.path().join("long.md");
        fs::write(&path, &text).unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(path));
        app.mode = Mode::Live;
        app.focus = Pane::Live;
        let pos = inkmark_view::ScrollPos {
            line: 120,
            frac: 0.0,
        };
        let parse = app.parse.output().clone();
        app.live.set_scroll_pos(&app.doc, &parse, pos);
        app.cycle_mode(&ctx);
        assert!(app.mode == Mode::Split);
        assert_eq!(app.code.scroll_pos(), pos);
    }

    fn drive(ctx: &egui::Context, app: &mut App, time: &mut f64, events: Vec<egui::Event>) {
        *time += 1.0 / 60.0;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_max(
                egui::Pos2::ZERO,
                egui::pos2(1000.0, 700.0),
            )),
            time: Some(*time),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(input, |ui| {
            app.handle_shortcuts(ui.ctx());
            app.poll_dialog(ui.ctx());
            app.hold_focus_for_dialogs(ui.ctx());
            app.editor(ui);
            app.new_file_ui(ui.ctx());
            app.rename_ui(ui.ctx());
            app.trash_ui(ui.ctx());
        });
        out.textures_delta.clear();
    }

    fn click_at(ctx: &egui::Context, app: &mut App, time: &mut f64, pos: egui::Pos2) {
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        drive(
            ctx,
            app,
            time,
            vec![egui::Event::PointerMoved(pos), button(true)],
        );
        drive(ctx, app, time, vec![button(false)]);
    }

    fn shortcut(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    fn wait_rows(ctx: &egui::Context, app: &mut App, time: &mut f64) {
        let start = std::time::Instant::now();
        loop {
            drive(ctx, app, time, vec![]);
            if app.browser.row_count() > 0 {
                return;
            }
            if start.elapsed() > std::time::Duration::from_secs(2) {
                panic!("sidebar listing did not arrive");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn sidebar_visibility_and_width_persist() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert!(app.sidebar.visible);
        let mut time = 0.0;
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::E, shift)],
        );
        assert!(!app.sidebar.visible);
        assert!(app.mode == Mode::Split);
        let stored = fs::read_to_string(dir.path().join("sidebar")).unwrap();
        assert!(stored.starts_with("0\n"), "{stored}");

        let ctx = egui::Context::default();
        let hidden = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert!(!hidden.sidebar.visible);

        // Show it again and drag the splitter. The new width is reloaded.
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::E, shift)],
        );
        assert!(app.sidebar.visible);
        let stored = dir.path().join("sidebar");
        let before = fs::read_to_string(&stored).unwrap();
        let start = egui::pos2(242.0, 80.0);
        let end = egui::pos2(320.0, 80.0);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::PointerMoved(end)],
        );
        let during = fs::read_to_string(&stored).unwrap();
        assert_eq!(during, before, "the width was written during the drag");
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(
            app.sidebar.width > 240.0,
            "width stayed {}",
            app.sidebar.width
        );
        let after = fs::read_to_string(&stored).unwrap();
        assert_ne!(after, before, "releasing the drag did not store the width");
        let ctx = egui::Context::default();
        let reloaded = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert!(reloaded.sidebar.visible);
        assert_eq!(reloaded.sidebar.width, app.sidebar.width);
    }

    #[test]
    fn open_folder_uses_the_dialog_channel_and_keeps_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let other = dir.path().join("other");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&other).unwrap();
        let file = notes.join("a.md");
        fs::write(&file, "keep\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            Some(file.clone()),
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        let (tx, rx) = mpsc::channel();
        app.dialog_hook = Some(tx);
        let mut time = 0.0;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(
                egui::Key::O,
                egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
            )],
        );
        assert_eq!(rx.try_recv().unwrap(), DialogKind::Folder);
        assert!(app.dialog.is_none());

        let (tx, dialog_rx) = mpsc::channel();
        app.dialog = Some(dialog_rx);
        tx.send(DialogResult::Folder(Some(other.clone()))).unwrap();
        app.poll_dialog(&ctx);
        assert_eq!(app.browser.root(), other);
        assert_eq!(app.doc.path(), Some(file.as_path()));
        assert_eq!(text(&app), "keep\n");
    }

    #[test]
    fn open_folder_while_hidden_shows_and_focuses_the_sidebar() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let other = dir.path().join("other");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&other).unwrap();
        let file = notes.join("a.md");
        fs::write(&file, "keep\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            Some(file.clone()),
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        let mut time = 0.0;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(
                egui::Key::E,
                egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
            )],
        );
        assert!(!app.sidebar.visible);

        let (tx, dialog_rx) = mpsc::channel();
        app.dialog = Some(dialog_rx);
        tx.send(DialogResult::Folder(Some(other.clone()))).unwrap();
        drive(&ctx, &mut app, &mut time, vec![]);
        assert!(app.sidebar.visible);
        assert_eq!(app.browser.root(), other);
        assert!(
            app.browser.has_focus(&ctx),
            "the sidebar did not take focus"
        );
        let stored = fs::read_to_string(dir.path().join("sidebar")).unwrap();
        assert!(stored.starts_with("1\n"), "{stored}");
        assert_eq!(app.doc.path(), Some(file.as_path()));
        assert_eq!(text(&app), "keep\n");
    }

    #[test]
    fn opening_from_the_sidebar_asks_before_dropping_edits() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        fs::create_dir(&notes).unwrap();
        let a = notes.join("a.md");
        let b = notes.join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            Some(a),
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        type_into(&mut app, "unsaved");
        let mut time = 0.0;
        wait_rows(&ctx, &mut app, &mut time);
        let rect = app.browser.row_rect(&b).expect("b.md should be on screen");
        click_at(&ctx, &mut app, &mut time, rect.center());
        assert_eq!(text(&app), "a\nunsaved");
        assert_eq!(app.confirm, Some(Confirm::OpenPath(b.clone())));

        // A clean document opens straight away, through the same path.
        app.confirm = None;
        app.doc = inkmark_buffer::Document::open(notes.join("a.md")).unwrap();
        click_at(&ctx, &mut app, &mut time, rect.center());
        assert_eq!(app.doc.path(), Some(b.as_path()));
        assert_eq!(text(&app), "b\n");
    }

    fn type_text(ctx: &egui::Context, app: &mut App, time: &mut f64, text: &str) {
        drive(ctx, app, time, vec![egui::Event::Text(text.to_string())]);
    }

    #[test]
    fn new_file_appends_md_refuses_an_existing_name_and_opens_it() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let chapter = notes.join("chapter");
        fs::create_dir_all(&chapter).unwrap();
        fs::write(notes.join("a.md"), "a\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            Some(notes.join("a.md")),
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        let mut time = 0.0;
        wait_rows(&ctx, &mut app, &mut time);
        let chapter_row = app
            .browser
            .row_rect(&chapter)
            .expect("chapter should be listed");
        click_at(&ctx, &mut app, &mut time, chapter_row.center());

        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::N, egui::Modifiers::COMMAND)],
        );
        let prompt = app.new_file.as_ref().expect("Ctrl+N opens the prompt");
        assert_eq!(prompt.dir, chapter);
        // A blank name is refused and writes nothing.
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert!(
            app.new_file
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("file name")
        );
        assert!(!chapter.join("inside.md").exists());

        type_text(&ctx, &mut app, &mut time, "inside");
        assert_eq!(app.new_file.as_ref().unwrap().name, "inside");
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let created = chapter.join("inside.md");
        assert_eq!(fs::read_to_string(&created).unwrap(), "");
        assert_eq!(app.doc.path(), Some(created.as_path()));
        assert_eq!(text(&app), "");
        assert!(app.new_file.is_none());

        // The header button targets the open file's folder and refuses a clash.
        drive(&ctx, &mut app, &mut time, vec![]);
        let button = app.browser.new_file_rect().expect("New file button");
        click_at(&ctx, &mut app, &mut time, button.center());
        assert_eq!(app.new_file.as_ref().unwrap().dir, chapter);
        type_text(&ctx, &mut app, &mut time, "inside");
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert!(
            app.new_file
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("already exists")
        );
        assert_eq!(fs::read_to_string(&created).unwrap(), "");
        assert_eq!(app.doc.path(), Some(created.as_path()));

        // A name that already has a Markdown extension is not given another.
        app.new_file.as_mut().unwrap().name = "also.md".into();
        app.new_file.as_mut().unwrap().error = None;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let also = chapter.join("also.md");
        assert!(also.is_file());
        assert!(!chapter.join("also.md.md").exists());
        assert_eq!(app.doc.path(), Some(also.as_path()));

        // Unsaved edits still go through the confirm prompt. The empty file stays.
        type_into(&mut app, "dirty");
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::N, egui::Modifiers::COMMAND)],
        );
        type_text(&ctx, &mut app, &mut time, "fresh");
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let fresh = chapter.join("fresh.md");
        assert_eq!(fs::read_to_string(&fresh).unwrap(), "");
        assert_eq!(text(&app), "dirty");
        assert_eq!(app.confirm, Some(Confirm::OpenPath(fresh)));

        // No selection: the new file lands in the root.
        let root_note = dir.path().join("root-note");
        fs::create_dir(&root_note).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            Some(root_note.clone()),
            recent::Recent::from_store(Some(dir.path().join("recent-root"))),
        );
        let mut time = 0.0;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::N, egui::Modifiers::COMMAND)],
        );
        assert_eq!(app.new_file.as_ref().unwrap().dir, root_note);
        type_text(&ctx, &mut app, &mut time, "plain");
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let plain = root_note.join("plain.md");
        assert!(plain.is_file());
        assert_eq!(app.doc.path(), Some(plain.as_path()));
        assert_eq!(text(&app), "");
    }

    #[test]
    fn a_sibling_change_updates_the_tree_and_the_open_file_keeps_its_banner() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        fs::create_dir(&notes).unwrap();
        let open = notes.join("open.md");
        fs::write(&open, "open\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            Some(open.clone()),
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        let mut time = 0.0;
        wait_rows(&ctx, &mut app, &mut time);
        fs::write(notes.join("other.md"), "o\n").unwrap();
        let start = std::time::Instant::now();
        loop {
            drive(&ctx, &mut app, &mut time, vec![]);
            if app
                .browser
                .row_names()
                .iter()
                .any(|name| name == "other.md")
            {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(1) {
                panic!(
                    "sibling did not appear within 1s: {:?}",
                    app.browser.row_names()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(text(&app), "open\n");
        assert!(app.banner.is_none());

        fs::write(&open, "changed by someone else\n").unwrap();
        app.next_disk_check = std::time::Instant::now();
        app.check_disk(&ctx);
        assert!(matches!(app.banner, Some(Banner::DiskChanged)));
        assert_eq!(text(&app), "open\n");
    }

    #[test]
    fn a_change_on_disk_is_announced_while_an_error_shows() {
        // From the first review: an error banner used to hide disk changes
        // until it was dismissed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "one\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(path.clone()));
        app.error = Some("Couldn't save: disk full".into());
        fs::write(&path, "changed elsewhere\n").unwrap();
        app.next_disk_check = Instant::now();
        app.check_disk(&ctx);
        assert!(matches!(app.banner, Some(Banner::DiskChanged)));
        assert_eq!(app.error.as_deref(), Some("Couldn't save: disk full"));
    }

    /// A trash that moves things into a folder of the test's own.
    struct FolderTrash(PathBuf);

    impl Trash for FolderTrash {
        fn trash(&self, path: &Path) -> std::io::Result<()> {
            fs::rename(path, self.0.join(path.file_name().unwrap()))
        }
    }

    #[test]
    fn renaming_the_open_file_keeps_its_unsaved_edits() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("draft.md");
        fs::write(&old, "one\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(old.clone()));
        type_into(&mut app, "unsaved\n");
        app.begin_rename(old.clone());
        assert_eq!(
            app.rename.as_ref().unwrap().name,
            "draft",
            "offered without .md"
        );
        app.rename.as_mut().unwrap().name = "final".into();
        let mut time = 0.0;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let new = dir.path().join("final.md");
        assert!(app.rename.is_none());
        assert_eq!(app.doc.path(), Some(new.as_path()));
        assert_eq!(text(&app), "one\nunsaved\n");
        assert!(app.doc.is_dirty());
        // The rename itself isn't a change on disk.
        app.next_disk_check = Instant::now();
        app.check_disk(&ctx);
        assert!(app.banner.is_none());
        assert_eq!(app.recent.entries()[0], new.canonicalize().unwrap());
        // Saving writes to the new name.
        app.save();
        assert_eq!(fs::read_to_string(&new).unwrap(), "one\nunsaved\n");
        assert!(!old.exists());
    }

    #[test]
    fn renaming_to_a_taken_name_says_so_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(dir.path().join("b.md"), "b\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        app.begin_rename(a.clone());
        app.rename.as_mut().unwrap().name = "b".into();
        let mut time = 0.0;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let prompt = app.rename.as_ref().expect("still asking");
        assert!(
            prompt.error.as_deref().unwrap().contains("b.md"),
            "{:?}",
            prompt.error
        );
        assert_eq!(fs::read_to_string(dir.path().join("b.md")).unwrap(), "b\n");
        assert_eq!(app.doc.path(), Some(a.as_path()));
    }

    #[test]
    fn moving_the_folder_of_the_open_file_takes_the_document_along() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let archive = dir.path().join("archive");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&archive).unwrap();
        let file = notes.join("a.md");
        fs::write(&file, "a\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(file.clone()));
        let (hook, kinds) = mpsc::channel();
        app.dialog_hook = Some(hook);
        app.apply_browser(&BrowserOutput {
            move_to: Some(notes.clone()),
            ..Default::default()
        });
        assert_eq!(kinds.try_recv().unwrap(), DialogKind::MoveTo(notes.clone()));
        app.dialog_hook = None;
        let (tx, rx) = mpsc::channel();
        app.dialog = Some(rx);
        tx.send(DialogResult::MoveTo(notes.clone(), Some(archive.clone())))
            .unwrap();
        app.poll_dialog(&ctx);
        let moved = archive.join("notes/a.md");
        assert!(moved.exists());
        assert_eq!(app.doc.path(), Some(moved.as_path()));
        app.next_disk_check = Instant::now();
        app.check_disk(&ctx);
        assert!(app.banner.is_none());

        // Dropping a row works the same way, and a refusal is an error.
        fs::write(dir.path().join("a.md"), "other\n").unwrap();
        app.apply_browser(&BrowserOutput {
            dropped: Some((moved.clone(), dir.path().to_path_buf())),
            ..Default::default()
        });
        assert!(
            app.error.as_deref().unwrap().contains("already exists"),
            "{:?}",
            app.error
        );
        assert_eq!(app.doc.path(), Some(moved.as_path()));
    }

    #[test]
    fn trashing_asks_first_and_keeps_the_open_text() {
        let dir = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        fs::write(&file, "a\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(file.clone()));
        app.trash = Box::new(FolderTrash(bin.path().to_path_buf()));
        app.apply_browser(&BrowserOutput {
            trash: Some(file.clone()),
            ..Default::default()
        });
        assert_eq!(app.trash_confirm, Some(file.clone()));
        let mut time = 0.0;
        // Escape cancels.
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Escape, egui::Modifiers::NONE)],
        );
        assert!(app.trash_confirm.is_none());
        assert!(file.exists());
        // Enter confirms.
        app.trash_confirm = Some(file.clone());
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert!(!file.exists());
        assert!(bin.path().join("a.md").exists());
        assert_eq!(text(&app), "a\n");
        assert!(matches!(app.banner, Some(Banner::DiskMissing)));
    }

    /// Frames until the parse is in and any jump from a followed link is done.
    fn settle(ctx: &egui::Context, app: &mut App, time: &mut f64) {
        let start = Instant::now();
        while !app.parse.is_settled() || app.pending_jump.is_some() {
            drive(ctx, app, time, vec![]);
            assert!(start.elapsed() < Duration::from_secs(5), "never settled");
            std::thread::sleep(Duration::from_millis(2));
        }
        drive(ctx, app, time, vec![]);
    }

    fn offset_of(app: &App, needle: &str) -> usize {
        text(app).find(needle).unwrap()
    }

    #[test]
    fn following_a_link_to_another_note_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "Intro.\n\nSee [the second part](b.md#second-part).\n").unwrap();
        fs::write(&b, "# First\n\ntext\n\n## Second part\n\nend\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        let clicked_from = offset_of(&app, "Intro") + 2;
        app.jump_to(clicked_from);
        drive(&ctx, &mut app, &mut time, vec![]);
        app.follow(offset_of(&app, "second part"));
        settle(&ctx, &mut app, &mut time);
        assert_eq!(app.doc.path(), Some(b.as_path()));
        assert_eq!(app.selection().head, offset_of(&app, "Second part"));
        // Alt+Left: back to a.md, where the caret was.
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::ArrowLeft, egui::Modifiers::ALT)],
        );
        settle(&ctx, &mut app, &mut time);
        assert_eq!(app.doc.path(), Some(a.as_path()));
        assert_eq!(app.selection().head, clicked_from);
        assert!(app.back.is_empty());
    }

    #[test]
    fn following_a_link_asks_before_dropping_edits() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "[next](b.md)\n").unwrap();
        fs::write(dir.path().join("b.md"), "# B\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        type_into(&mut app, "draft");
        drive(&ctx, &mut app, &mut time, vec![]);
        app.follow(1);
        assert_eq!(
            app.confirm,
            Some(Confirm::OpenPath(dir.path().join("b.md")))
        );
        assert_eq!(app.doc.path(), Some(a.as_path()));
        assert!(text(&app).ends_with("draft"));
    }

    #[test]
    fn web_links_open_outside_and_other_targets_explain_themselves() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(
            &a,
            "[site](https://example.org/x) [pic](photo.png) [js](javascript:x) [gone](#nowhere)\n",
        )
        .unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        app.follow(offset_of(&app, "site"));
        assert_eq!(app.opened_urls, vec!["https://example.org/x".to_owned()]);
        for (needle, says) in [
            ("pic", "photo.png"),
            ("js]", "javascript"),
            ("gone", "#nowhere"),
        ] {
            app.hint = None;
            app.follow(offset_of(&app, needle));
            let hint = app
                .hint
                .as_ref()
                .map(|(h, _)| h.clone())
                .unwrap_or_default();
            assert!(hint.contains(says), "{needle}: {hint:?}");
        }
        assert_eq!(app.doc.path(), Some(a.as_path()));
        assert!(
            app.back.is_empty(),
            "nothing followed, nothing to go back to"
        );
    }

    #[test]
    fn a_footnote_reference_jumps_to_its_note_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "Claim[^1] here.\n\nMore.\n\n[^1]: The source.\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        app.jump_to(2);
        app.follow(offset_of(&app, "[^1]"));
        assert_eq!(app.selection().head, offset_of(&app, "The source"));
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::ArrowLeft, egui::Modifiers::ALT)],
        );
        assert_eq!(app.selection().head, 2);
    }
}
