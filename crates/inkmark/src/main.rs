use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Key, Modifiers, Rect, RichText, Stroke, UiBuilder, ViewportCommand, pos2,
};
use inkmark_buffer::{DiskStatus, Document, LineEnding, OpenError, Selection};
use inkmark_files::{
    Launch, NewFileError, NewFolderError, SystemTrash, Trash, choose_root, create_new_file,
    create_new_folder, move_into, rename,
};
use inkmark_parse::{GfmParser, ParseState};
use inkmark_text::Fonts;
use inkmark_view::keys::{self, Action, Scope};
use inkmark_view::{BrowserOutput, CodeView, FileBrowser, LiveView, theme};

/// Font sizes, in points, unless config.toml sets them, and line height as
/// a multiple of the size.
const CODE_SIZE: f32 = 14.0;
const TEXT_SIZE: f32 = 16.0;
const CODE_LINE: f32 = 21.0 / 14.0;
const TEXT_LINE: f32 = 26.0 / 16.0;

/// How long a key hint stays in the status bar.
const HINT_TIME: Duration = Duration::from_secs(4);

/// How often we look for changes made to the file by other programs.
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd", "txt"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Code,
    Live,
}

mod config;
mod findbar;
mod folder_search;
mod gotoline;
mod layout;
mod links;
mod measure;
mod os_theme;
mod outline;
mod recent;
mod session;

use findbar::FindBar;
use folder_search::{FolderSearch, SearchOpen};
use gotoline::GoToLine;
pub(crate) use layout::Mode;
mod sidebar;

const USAGE: &str = "\
Usage: inkmark [FILE | FOLDER]

  FILE     open it, browsing its folder (a missing path becomes a new file)
  FOLDER   browse it, with nothing open
           (no argument: browse the current directory)

Options:
  -h, --help     show this help
  -V, --version  show the version
  --list-keys    print key bindings, as config.toml
  --             treat what follows as a path, even if it starts with -";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run(Option<PathBuf>),
    Help,
    Version,
    ListKeys,
}

fn parse_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Command, String> {
    let mut path = None;
    let mut options_done = false;
    for arg in args {
        let text = arg.to_str().unwrap_or_default();
        match text {
            "--" if !options_done => options_done = true,
            "-h" | "--help" if !options_done => return Ok(Command::Help),
            "-V" | "--version" if !options_done => return Ok(Command::Version),
            "--list-keys" if !options_done => return Ok(Command::ListKeys),
            _ if !options_done && text.starts_with('-') && text.len() > 1 => {
                return Err(format!("unknown option {text}"));
            }
            _ if path.is_some() => return Err("only one file or folder can be opened".into()),
            _ => path = Some(PathBuf::from(arg)),
        }
    }
    Ok(Command::Run(path))
}

fn main() -> eframe::Result {
    let start = Instant::now();
    let path = match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Run(path)) => path,
        Ok(Command::Help) => {
            println!("inkmark {}\n\n{USAGE}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(Command::Version) => {
            println!("inkmark {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Ok(Command::ListKeys) => {
            // stdout is pasteable TOML. Problems (a bad file, a bad chord)
            // go to stderr and don't change the exit status.
            let (text, problems) = config::list_keys();
            print!("{text}");
            for problem in problems {
                eprintln!("{problem}");
            }
            return Ok(());
        }
        Err(message) => {
            eprintln!("inkmark: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    session::init();
    // `with_active` is ignored on Wayland. The e2e script asks Hyprland not
    // to focus a window whose app id is `inkmark-e2e`.
    let e2e = std::env::var_os("INKMARK_E2E").is_some();
    let viewport = egui::ViewportBuilder::default()
        .with_title(if e2e { "inkmark e2e" } else { "inkmark" })
        .with_app_id(if e2e { "inkmark-e2e" } else { "inkmark" })
        .with_inner_size(if e2e { [480.0, 320.0] } else { [1200.0, 800.0] })
        .with_active(!e2e);
    let options = eframe::NativeOptions {
        viewport,
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
    let result = eframe::run_native(
        "inkmark",
        options,
        Box::new(|cc| {
            let mut app = App::new(&cc.egui_ctx, path);
            app.measure = measure::Measure::from_env(start);
            Ok(Box::new(app))
        }),
    );
    match &result {
        Ok(()) => session::line("event_loop returned ok"),
        Err(err) => session::line(&format!("event_loop returned error: {err}")),
    }
    result
}

/// The chrome is Hack, which has the folder and unsaved marks. Hack is
/// inserted in front of egui's own proportional list, so those fallback
/// faces stay whatever this egui version ships. The document is
/// cosmic-text and is not affected.
fn install_ui_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        family.insert(0, "Hack".to_owned());
    }
    ctx.set_fonts(fonts);
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

/// How a navigation got where it's going, for the history.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Travel {
    /// Following a link: a new way forward.
    Follow,
    /// Alt+Left or Alt+Right to this place (taken off its stack).
    Back(Place),
    Forward(Place),
}

/// What to do once a file opened by following a link is parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Jump {
    Anchor(String),
    Offset(usize),
    /// A folder-search match. `text` is what the range covered when it was found.
    Select {
        range: std::ops::Range<usize>,
        text: String,
    },
}

#[derive(Clone)]
struct RenamePrompt {
    path: PathBuf,
    name: String,
    error: Option<String>,
    /// Select this many leading chars when the prompt first shows.
    select: Option<usize>,
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
    /// Places gone back from, most recent last (Alt+Right).
    forward: Vec<Place>,
    /// A history change waiting on a file being opened: committed once that
    /// file is open, undone if the open was cancelled or failed.
    pending_history: Option<(PathBuf, Place, Travel)>,
    /// Where to put the caret once the file being opened has been parsed.
    pending_jump: Option<(PathBuf, Jump)>,
    /// Tests record URLs here instead of starting a browser.
    #[cfg(test)]
    opened_urls: Vec<String>,
    dialog: Option<Receiver<DialogResult>>,
    confirm: Option<Confirm>,
    close_after_save: bool,
    close_allowed: bool,
    /// Whether the window had keyboard focus last frame.
    focused: Option<bool>,
    /// Last `alive` line in the session log.
    last_beat: Instant,
    started: Instant,
    /// Set from `INKMARK_E2E_QUIT_AFTER`. Absent in normal use.
    quit_after: Option<Duration>,
    /// A close request was already written to the session log.
    logged_close: bool,
    /// `sending close` was already written.
    logged_send: bool,
    next_disk_check: Instant,
    title: String,
    measure: Option<measure::Measure>,
    recent: recent::Recent,
    /// The recent-files list is open, with this entry selected.
    recent_list: Option<usize>,
    browser: FileBrowser,
    sidebar: sidebar::Sidebar,
    /// Mode, minimaps, the split, and the outline width, restored on open.
    layout: layout::Layout,
    /// Drawn outline width where the current drag began.
    outline_drag_from: Option<f32>,
    /// Drawn split position where the current drag began.
    split_drag_from: Option<f32>,
    /// Asks for a name, then creates the file and opens it.
    new_file: Option<NewFilePrompt>,
    /// Asks for a name, then creates the folder and selects it.
    new_folder: Option<NewFilePrompt>,
    /// F1. The chords in effect, in the same order as `--list-keys`.
    show_keys: bool,
    /// Asks for a new name for this file or folder.
    rename: Option<RenamePrompt>,
    /// Asks before moving this file or folder to the trash.
    trash_confirm: Option<PathBuf>,
    /// Where Move to Trash sends things; tests use their own.
    trash: Box<dyn Trash>,
    /// The desktop's colors, followed live (fixed in tests).
    os_theme: os_theme::OsTheme,
    /// The font database both panes share, for applying font settings.
    fonts: inkmark_text::SharedFonts,
    /// config.toml and fontconfig's file, watched for font changes (none
    /// in tests), with their last modification times.
    font_files: Vec<Option<PathBuf>>,
    font_stamps: Vec<Option<std::time::SystemTime>>,
    /// The last config.toml that read cleanly, kept through a bad read.
    settings: config::Settings,
    /// Shortcuts for the window, the sidebar and both panes.
    keys: keys::KeyMap,
    /// Find and replace in the open document. Closed, it still remembers
    /// the query so F3 can repeat it.
    find: FindBar,
    /// Ctrl+G. Closed, it keeps nothing: the next open starts empty.
    goto: GoToLine,
    /// Ctrl+Shift+F. File names, then matches inside those files.
    search: FolderSearch,
    /// Heading rows for the outline, rebuilt when the parse revision changes.
    outline: outline::Outline,
    /// The error banner text the settings put up, to take down once fixed.
    settings_error: Option<String>,
    /// A dialog took keyboard focus from the panes last frame.
    modal_was_open: bool,
    /// Tests receive the dialog kind instead of opening a portal window.
    #[cfg(test)]
    dialog_hook: Option<mpsc::Sender<DialogKind>>,
}

impl App {
    fn new(ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        let mut app = Self::with_recent(ctx, path, recent::Recent::load());
        app.os_theme = os_theme::OsTheme::follow(ctx);
        app.font_files = vec![config::config_path(), config::fontconfig_path()];
        app.apply_settings();
        app
    }

    fn with_recent(ctx: &egui::Context, path: Option<PathBuf>, recent: recent::Recent) -> Self {
        install_ui_font(ctx);
        // One font database and glyph atlas for both panes.
        let fonts = Fonts::shared(ctx);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let launch = choose_root(path.as_deref(), &cwd);
        let root = match &launch {
            Launch::File { root, .. } | Launch::Folder { root } => root.clone(),
        };
        let state_dir = recent
            .store_path()
            .and_then(|store| store.parent().map(Path::to_path_buf));
        let sidebar_store = state_dir.as_ref().map(|dir| dir.join("sidebar"));
        let layout_store = state_dir.as_ref().map(|dir| dir.join("layout"));
        let layout = layout::Layout::load(layout_store);
        let mode = layout.mode;
        let code_minimap = layout.code_minimap;
        let live_minimap = layout.live_minimap;
        let search_ctx = ctx.clone();
        let mut app = Self {
            doc: Document::default(),
            code: CodeView::with_fonts(fonts.clone(), egui::Id::new("code_view")),
            live: LiveView::with_fonts(fonts.clone(), egui::Id::new("live_view")),
            mode,
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
            forward: Vec::new(),
            pending_history: None,
            pending_jump: None,
            #[cfg(test)]
            opened_urls: Vec::new(),
            dialog: None,
            confirm: None,
            close_after_save: false,
            close_allowed: false,
            focused: None,
            last_beat: Instant::now(),
            started: Instant::now(),
            quit_after: session::quit_after(),
            logged_close: false,
            logged_send: false,
            next_disk_check: Instant::now() + DISK_CHECK_INTERVAL,
            title: String::new(),
            measure: None,
            recent,
            recent_list: None,
            browser: FileBrowser::new(root.clone()),
            sidebar: sidebar::Sidebar::load(sidebar_store),
            layout,
            outline_drag_from: None,
            split_drag_from: None,
            new_file: None,
            new_folder: None,
            show_keys: false,
            rename: None,
            trash_confirm: None,
            trash: Box::new(SystemTrash),
            os_theme: os_theme::OsTheme::fixed(),
            fonts: fonts.clone(),
            font_files: Vec::new(),
            font_stamps: Vec::new(),
            settings: config::Settings::default(),
            keys: keys::KeyMap::builtin(),
            find: FindBar::default(),
            goto: GoToLine::default(),
            search: FolderSearch::new(root, move || search_ctx.request_repaint()),
            outline: outline::Outline::default(),
            settings_error: None,
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
        app.code.show_minimap = code_minimap;
        app.live.show_minimap = live_minimap;
        app.install_keys();
        // A live-only window keeps the keyboard on the pane that is showing.
        if app.mode == Mode::Live {
            app.focus = Pane::Live;
            app.live.request_focus(ctx);
        } else {
            app.code.request_focus(ctx);
        }
        app
    }

    /// Copies the current shortcuts onto the sidebar and both panes.
    fn install_keys(&mut self) {
        self.code.set_keys(self.keys.clone());
        self.live.set_keys(self.keys.clone());
        self.browser.set_keys(self.keys.clone());
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

    /// Reads config.toml and the system fonts and applies them: config
    /// first, then fontconfig's monospace and sans-serif families.
    fn apply_settings(&mut self) {
        self.font_stamps = config::stamps(&self.font_files);
        let path = self.font_files.first().cloned().flatten();
        self.apply_settings_from(path.as_deref(), config::system_font);
    }

    fn apply_settings_from(
        &mut self,
        config_file: Option<&Path>,
        system_font: impl Fn(&str) -> Option<String>,
    ) {
        let mut problems = Vec::new();
        // No file: defaults. A file that can't be read or parsed (perhaps
        // half-written): say so and keep the last good settings.
        let read = match config_file.map(std::fs::read_to_string) {
            None => Ok(None),
            Some(Ok(text)) => config::parse(&text).map(Some),
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Some(Err(e)) => Err(e.to_string()),
        };
        match read {
            Ok(settings) => {
                self.settings = settings.unwrap_or_default();
                // A bad chord is reported and that action keeps its default.
                // The rest of the file, fonts included, still applies.
                problems.extend(self.settings.key_problems.iter().cloned());
                self.keys = self.settings.keys.clone();
                self.install_keys();
            }
            Err(e) => problems.push(format!(
                "Couldn't read config.toml: {e} (keeping the previous settings)"
            )),
        }
        let settings = self.settings.clone();
        // A font named in the config that isn't installed falls back to the
        // system's, as if it weren't named.
        let mut missing = Vec::new();
        let mut pick = |configured: Option<&String>, alias: &str| match configured {
            Some(name) if self.fonts.borrow().has_family(name) => Some(name.clone()),
            Some(name) => {
                missing.push(name.clone());
                system_font(alias)
            }
            None => system_font(alias),
        };
        let code = pick(settings.code_font.as_ref(), "monospace");
        let text = pick(settings.text_font.as_ref(), "sans-serif");
        missing.extend(
            self.fonts
                .borrow_mut()
                .set_families(code.as_deref(), text.as_deref()),
        );
        if !missing.is_empty() {
            problems.push(format!(
                "Font not installed: {} (using the system font)",
                missing.join(", ")
            ));
        }
        let code_size = settings.code_size.unwrap_or(CODE_SIZE);
        let text_size = settings.text_size.unwrap_or(TEXT_SIZE);
        self.code.font_size = code_size;
        self.code.line_height = (code_size * CODE_LINE).round();
        self.live.font_size = text_size;
        self.live.line_height = (text_size * TEXT_LINE).round();
        // Show what's wrong; once nothing is, take down only our own banner.
        if problems.is_empty() {
            if self.error.is_some() && self.error == self.settings_error {
                self.error = None;
            }
            self.settings_error = None;
        } else {
            let message = problems.join(". ");
            self.error = Some(message.clone());
            self.settings_error = Some(message);
        }
    }

    fn check_disk(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if now >= self.next_disk_check {
            self.next_disk_check = now + DISK_CHECK_INTERVAL;
            if !self.font_files.is_empty() && config::stamps(&self.font_files) != self.font_stamps {
                self.apply_settings();
            }
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
        let mode_before = self.mode;
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
        if self.mode != mode_before {
            self.remember_layout();
        }
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
        self.remember_layout();
    }

    /// Writes the mode, both minimaps, and the widths. A resize that only
    /// draws a pane narrower does not call this.
    fn remember_layout(&mut self) {
        self.layout.mode = self.mode;
        self.layout.code_minimap = self.code.show_minimap;
        self.layout.live_minimap = self.live.show_minimap;
        self.layout.save();
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // App chords are taken before a pane sees the key. Matching is
        // exact, so a pane chord that adds Shift or Alt (Ctrl+Alt+1 is a
        // heading, Alt+Shift+Left moves a table column) is left for the pane.
        let actions = ctx.input_mut(|i| self.keys.consume(i, Scope::App));
        for action in actions {
            self.run_app_action(ctx, action);
        }
    }

    fn run_app_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::ToggleSidebar => self.toggle_sidebar(ctx),
            Action::ToggleOutline => {
                self.layout.outline_visible = !self.layout.outline_visible;
                self.remember_layout();
            }
            Action::RecentFiles => {
                self.show_keys = false;
                self.recent_list = match self.recent_list {
                    Some(_) => None,
                    None => {
                        self.new_file = None;
                        self.new_folder = None;
                        Some(0)
                    }
                };
            }
            Action::ToggleMinimap => {
                // Each pane keeps its own minimap setting.
                match self.focus {
                    Pane::Code => self.code.show_minimap ^= true,
                    Pane::Live => self.live.show_minimap ^= true,
                }
                self.remember_layout();
            }
            Action::CycleMode => self.cycle_mode(ctx),
            Action::FocusCode => self.focus_pane(ctx, Pane::Code),
            Action::FocusLive => self.focus_pane(ctx, Pane::Live),
            Action::Back => self.go_back(),
            Action::Forward => self.go_forward(),
            Action::OpenFolder => self.spawn_dialog(DialogKind::Folder),
            Action::SaveAs => self.spawn_dialog(DialogKind::SaveAs),
            Action::Save => self.save(),
            Action::OpenFile => {
                if self.doc.is_dirty() {
                    self.confirm = Some(Confirm::Open);
                } else {
                    self.spawn_dialog(DialogKind::Open);
                }
            }
            Action::NewFile => self.begin_new_file(),
            Action::NewFolder => self.begin_new_folder(),
            Action::ShowKeys => self.toggle_keys(ctx),
            Action::Find => self.open_find(ctx, false),
            Action::Replace => self.open_find(ctx, true),
            Action::FindNext => self.find_move(true),
            Action::FindPrevious => self.find_move(false),
            Action::GoToLine => self.open_goto(ctx),
            Action::SearchFolder => self.open_folder_search(ctx),
            _ => {}
        }
    }

    /// Ctrl+F / Ctrl+H. A dialog already owns the keyboard, so find waits.
    fn open_find(&mut self, ctx: &egui::Context, replace: bool) {
        if self.modal_open() {
            return;
        }
        self.goto.close();
        self.search.close();
        let selection = self.selection();
        let range = selection.range();
        let seed = FindBar::seed(self.doc.slice(range.clone()).as_ref());
        self.find.open(range.start, seed, replace);
        // The bar takes the keys; the caret comes back when it closes.
        self.code.release_focus(ctx);
        self.live.release_focus(ctx);
    }

    /// Ctrl+G. A dialog already owns the keyboard, so the prompt waits.
    fn open_goto(&mut self, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }
        self.find.close();
        self.search.close();
        self.goto.open();
        self.code.release_focus(ctx);
        self.live.release_focus(ctx);
    }

    /// Ctrl+Shift+F. A hidden sidebar is shown, and the query takes the keys.
    fn open_folder_search(&mut self, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }
        self.find.close();
        self.goto.close();
        if !self.sidebar.visible {
            self.sidebar.visible = true;
            self.sidebar.save();
        }
        self.search.open();
        self.code.release_focus(ctx);
        self.live.release_focus(ctx);
    }

    fn find_move(&mut self, next: bool) {
        let selection = self.selection();
        let moved = if next {
            self.find.goto_next(&self.doc, selection)
        } else {
            self.find.goto_prev(&self.doc, selection)
        };
        if let Some(sel) = moved {
            self.show_match(sel);
        }
    }

    fn toggle_sidebar(&mut self, ctx: &egui::Context) {
        self.sidebar.visible = !self.sidebar.visible;
        self.sidebar.save();
        if self.sidebar.visible {
            self.browser.request_focus();
        } else {
            self.focus_pane(ctx, self.focus);
        }
    }

    /// F1 opens the list, and F1 again closes it. Another dialog keeps it shut.
    fn toggle_keys(&mut self, ctx: &egui::Context) {
        if self.show_keys {
            self.show_keys = false;
            return;
        }
        if self.modal_open() {
            return;
        }
        self.recent_list = None;
        self.show_keys = true;
        self.code.release_focus(ctx);
        self.live.release_focus(ctx);
    }

    fn modal_open(&self) -> bool {
        self.recent_list.is_some()
            || self.confirm.is_some()
            || self.new_file.is_some()
            || self.new_folder.is_some()
            || self.show_keys
            || self.rename.is_some()
            || self.trash_confirm.is_some()
            || self.dialog.is_some()
    }

    /// Ctrl+Z and Ctrl+Y while the bar holds the keyboard. A field would
    /// undo its own text, and a button click leaves the panes unfocused.
    fn find_history_keys(&mut self, ctx: &egui::Context) {
        if !self.find.is_open() || self.modal_open() {
            return;
        }
        let field = self.find.field_focused(ctx);
        let pane = self.code.has_focus(ctx) || self.live.has_focus(ctx);
        if pane && !field {
            return;
        }
        let actions = ctx.input_mut(|input| {
            let mut found = Vec::new();
            input.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                else {
                    return true;
                };
                match self.keys.find(*key, *modifiers, Scope::Editor) {
                    Some(action @ (Action::Undo | Action::Redo)) => {
                        found.push(action);
                        false
                    }
                    _ => true,
                }
            });
            found
        });
        for action in actions {
            let restored = match action {
                Action::Undo => self.doc.undo(),
                Action::Redo => self.doc.redo(),
                _ => None,
            };
            if let Some(selection) = restored {
                self.show_match(selection);
                self.find.sync_count(&self.doc, selection);
            }
        }
    }

    /// Selects `selection` in the focused pane and mirrors it to the other.
    fn show_match(&mut self, selection: Selection) {
        match self.focus {
            Pane::Code => {
                self.code.set_selection(selection);
                self.live.mirror_selection(selection);
            }
            Pane::Live => {
                self.live.set_selection(selection);
                self.code.mirror_selection(selection);
            }
        }
    }

    fn guard_close(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|i| i.viewport().close_requested());
        if requested && !self.logged_close {
            let cancel = self.doc.is_dirty() && !self.close_allowed;
            session::line(&format!(
                "close_requested dirty={} cancel={cancel}",
                self.doc.is_dirty()
            ));
            self.logged_close = true;
        }
        if !requested {
            self.logged_close = false;
        }
        if requested && self.doc.is_dirty() && !self.close_allowed {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Close);
        }
        if self.close_allowed {
            if !self.logged_send {
                session::line("sending close");
                self.logged_send = true;
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    /// Heartbeat, focus changes, and the e2e harness's timed quit.
    fn note_session(&mut self, ctx: &egui::Context) {
        let path = self.doc.path().map(|path| path.display().to_string());
        let path = path.as_deref().unwrap_or("untitled");
        if self.focused.is_none() {
            session::line(&format!("first_frame path={path}"));
        }
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(false);
        if self.focused != Some(focused) {
            session::line(&format!("focus {focused}"));
            self.focused = Some(focused);
        }
        if self.last_beat.elapsed() >= session::heartbeat_every() {
            self.last_beat = Instant::now();
            session::line(&format!(
                "alive focused={focused} dirty={} path={path}",
                self.doc.is_dirty()
            ));
        }
        if let Some(after) = self.quit_after {
            let left = after.saturating_sub(self.started.elapsed());
            if left.is_zero() && !self.close_allowed {
                session::line("e2e_quit");
                self.close_allowed = true;
            } else if !left.is_zero() {
                // Wake once to close. Idle runs leave the repaint schedule alone.
                ctx.request_repaint_after(left);
            }
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
                ui.label(RichText::new(message).color(theme::current(ui.ctx()).error));
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

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        let head = self.selection().head;
        let line = self.doc.byte_to_line(head);
        let column = self
            .doc
            .slice(self.doc.line_to_byte(line)..head)
            .chars()
            .count()
            + 1;
        let encoding = self.doc.encoding();
        let files_tip = binding_tip(&self.keys, Action::ToggleSidebar);
        let outline_tip = binding_tip(&self.keys, Action::ToggleOutline);
        let mut toggle_files = false;
        let mut toggle_outline = false;
        ui.horizontal(|ui| {
            // « and » are drawn by Hack. They sit at the outer edges.
            let files = ui
                .add(egui::Button::new("«").small())
                .on_hover_text(&files_tip);
            if files.clicked() {
                toggle_files = true;
            }
            let path = self
                .doc
                .path()
                .map_or_else(|| "untitled".into(), |p| p.display().to_string());
            ui.label(path);
            if self.doc.is_dirty() {
                ui.label("●");
            }
            if let Some((hint, _)) = &self.hint {
                ui.label(RichText::new(hint).color(theme::current(ui.ctx()).hint));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let outline = ui
                    .add(egui::Button::new("»").small())
                    .on_hover_text(&outline_tip);
                if outline.clicked() {
                    toggle_outline = true;
                }
                ui.label(if encoding.bom { "UTF-8 BOM" } else { "UTF-8" });
                ui.label(match encoding.line_ending {
                    LineEnding::Lf => "LF",
                    LineEnding::CrLf => "CRLF",
                });
                ui.label(format!("Ln {}, Col {column}", line + 1));
            });
        });
        let ctx = ui.ctx().clone();
        if toggle_files {
            self.toggle_sidebar(&ctx);
        }
        if toggle_outline {
            self.layout.outline_visible = !self.layout.outline_visible;
            self.remember_layout();
        }
    }

    /// While a dialog is open the panes don't get keys (typing mustn't edit
    /// the document behind it); focus returns when it closes.
    fn hold_focus_for_dialogs(&mut self, ctx: &egui::Context) {
        let open = self.recent_list.is_some()
            || self.confirm.is_some()
            || self.new_file.is_some()
            || self.new_folder.is_some()
            || self.show_keys
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
        if self.new_file.is_some()
            || self.new_folder.is_some()
            || self.confirm.is_some()
            || self.rename.is_some()
            || self.trash_confirm.is_some()
        {
            return;
        }
        self.recent_list = None;
        self.show_keys = false;
        self.new_file = Some(NewFilePrompt {
            dir: self.browser.new_file_dir(),
            name: String::new(),
            error: None,
        });
    }

    fn begin_new_folder(&mut self) {
        if self.new_folder.is_some()
            || self.new_file.is_some()
            || self.confirm.is_some()
            || self.rename.is_some()
            || self.trash_confirm.is_some()
        {
            return;
        }
        self.recent_list = None;
        self.show_keys = false;
        self.new_folder = Some(NewFilePrompt {
            dir: self.browser.new_file_dir(),
            name: String::new(),
            error: None,
        });
    }

    /// Creates the named folder and selects it. The open document stays put.
    fn submit_new_folder(&mut self) {
        let Some(prompt) = self.new_folder.clone() else {
            return;
        };
        match create_new_folder(&prompt.dir, &prompt.name) {
            Ok(path) => {
                self.new_folder = None;
                self.browser.note_dir_created(&path);
            }
            Err(NewFolderError::Exists(_)) => {
                if let Some(prompt) = &mut self.new_folder {
                    prompt.error = Some("A folder with that name already exists.".into());
                }
            }
            Err(NewFolderError::Empty) => {
                if let Some(prompt) = &mut self.new_folder {
                    prompt.error = Some("Enter a folder name.".into());
                }
            }
            Err(NewFolderError::Invalid) => {
                if let Some(prompt) = &mut self.new_folder {
                    prompt.error =
                        Some("A folder name can't contain / or \\, or be . or ..".into());
                }
            }
            Err(NewFolderError::Io(message)) => {
                self.new_folder = None;
                self.error = Some(format!("Couldn't create the folder: {message}"));
            }
        }
    }

    fn new_folder_ui(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.new_folder.clone() else {
            return;
        };
        let detail = prompt.dir.display().to_string();
        let (submit, cancel) = name_modal(
            ctx,
            PromptText {
                id: "new_folder",
                heading: "New folder",
                detail: &detail,
                action: "Create",
            },
            &mut prompt.name,
            prompt.error.as_deref(),
            None,
        );
        if cancel {
            self.new_folder = None;
            return;
        }
        self.new_folder = Some(prompt);
        if submit {
            self.submit_new_folder();
        }
    }

    /// The chords in effect. Escape or F1 closes the list.
    fn keys_ui(&mut self, ctx: &egui::Context) {
        if !self.show_keys {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.show_keys = false;
            return;
        }
        let rows: Vec<(String, String)> = Action::ALL
            .into_iter()
            .map(|action| {
                let chord = self.keys.shortcut_text(action);
                let chord = if chord.is_empty() {
                    "unbound".to_owned()
                } else {
                    chord
                };
                (chord, action.description().to_owned())
            })
            .collect();
        egui::Modal::new(egui::Id::new("keys")).show(ctx, |ui| {
            ui.set_min_width(520.0);
            ui.heading("Key bindings");
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| {
                    egui::Grid::new("key_bindings")
                        .striped(true)
                        .show(ui, |ui| {
                            for (chord, description) in &rows {
                                ui.label(RichText::new(chord).strong());
                                ui.label(description);
                                ui.end_row();
                            }
                        });
                });
            ui.add_space(6.0);
            ui.label(RichText::new("Esc closes").small().weak());
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
            PromptText {
                id: "new_file",
                heading: "New file",
                detail: &detail,
                action: "Create",
            },
            &mut prompt.name,
            prompt.error.as_deref(),
            None,
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

    /// Rename… for a sidebar entry: the whole name, with the part before
    /// a file's extension selected, so typing keeps the extension.
    fn begin_rename(&mut self, path: PathBuf) {
        if self.confirm.is_some()
            || self.new_file.is_some()
            || self.new_folder.is_some()
            || self.show_keys
        {
            return;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let stem_chars = match path.extension() {
            Some(ext) if !path.is_dir() => {
                name.chars().count() - ext.to_string_lossy().chars().count() - 1
            }
            _ => name.chars().count(),
        };
        self.recent_list = None;
        self.rename = Some(RenamePrompt {
            path,
            name,
            error: None,
            select: Some(stem_chars),
        });
    }

    fn rename_ui(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.rename.clone() else {
            return;
        };
        let detail = prompt.path.display().to_string();
        let (submit, cancel) = name_modal(
            ctx,
            PromptText {
                id: "rename",
                heading: "Rename",
                detail: &detail,
                action: "Rename",
            },
            &mut prompt.name,
            prompt.error.as_deref(),
            prompt.select.take(),
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
        // Places to go back to, and a jump waiting on a file, follow too.
        let follow = |p: &mut PathBuf| {
            if let Ok(rest) = p.strip_prefix(old) {
                *p = if rest.as_os_str().is_empty() {
                    new.to_path_buf()
                } else {
                    new.join(rest)
                };
            }
        };
        for place in self.back.iter_mut().chain(self.forward.iter_mut()) {
            if let Some(path) = &mut place.path {
                follow(path);
            }
        }
        if let Some((path, _)) = &mut self.pending_jump {
            follow(path);
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
    /// Sidebar on the left, heading outline on the right. Each one can be
    /// hidden on its own. Hiding the sidebar leaves the outline up.
    fn editor(&mut self, ui: &mut egui::Ui) {
        self.browser
            .set_current(self.doc.path().map(|path| path.to_path_buf()));
        self.browser.set_dirty(self.doc.is_dirty());
        if !self.sidebar.visible {
            self.browser.poll_listings(ui.ctx());
        }
        let rect = ui.available_rect_before_wrap();
        let gap = outline::GAP;
        let wanted = self.sidebar.visible.then_some(
            self.sidebar
                .width
                .clamp(sidebar::MIN_WIDTH, sidebar::MAX_WIDTH),
        );
        let outline_wanted = self
            .layout
            .outline_visible
            .then_some(self.layout.outline_width);
        let (sidebar_w, outline_w) = outline::column_widths(rect.width(), wanted, outline_wanted);
        let mut cursor = rect.left();
        if self.sidebar.visible {
            let left = Rect::from_min_max(rect.min, pos2(cursor + sidebar_w, rect.bottom()));
            cursor = left.right();
            let mut output = BrowserOutput::default();
            let root = self.browser.root().to_path_buf();
            let show_all = self.browser.show_all();
            let modal = self.modal_open();
            let search_open = self.search.is_open();
            let (hit, closed) = ui
                .scope_builder(UiBuilder::new().max_rect(left), |ui| {
                    let bar = if search_open {
                        Some(self.search.show_bar(ui, &root, show_all, modal))
                    } else {
                        None
                    };
                    let body_top = bar.as_ref().map(|bar| bar.body_top).unwrap_or(left.top());
                    let body = Rect::from_min_max(
                        pos2(left.left(), body_top.min(left.bottom())),
                        left.max,
                    );
                    let mut hit = None;
                    ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
                        if bar.as_ref().is_some_and(|bar| bar.showing_results) {
                            self.browser.poll_listings(ui.ctx());
                            hit = self.search.show_results(ui);
                        } else {
                            output = self.browser.show(ui);
                        }
                    });
                    let mut opened = hit;
                    let mut closed = false;
                    if let Some(bar) = bar {
                        closed = bar.closed;
                        if opened.is_none() {
                            opened = bar.open;
                        }
                    }
                    (opened, closed)
                })
                .inner;
            self.apply_browser(&output);
            if output.search {
                self.open_folder_search(ui.ctx());
            }
            if let Some(hit) = hit {
                self.open_search_hit(hit);
            }
            if closed {
                self.search.close();
                self.focus_pane(ui.ctx(), self.focus);
            }
            let handle =
                Rect::from_min_max(pos2(cursor, rect.top()), pos2(cursor + gap, rect.bottom()));
            cursor = handle.right();
            let handle_resp =
                ui.interact(handle, egui::Id::new("sidebar_split"), egui::Sense::drag());
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
            // The gap would otherwise show the window's clear color.
            let colors = theme::current(ui.ctx());
            ui.painter().rect_filled(handle, 0.0, colors.background);
            ui.painter().vline(
                handle.center().x,
                handle.y_range(),
                Stroke::new(1.0, colors.divider),
            );
        }

        let panes_right = if outline_w > 1.0 {
            rect.right() - outline_w - gap
        } else {
            rect.right()
        };
        let panes = Rect::from_min_max(pos2(cursor, rect.top()), pos2(panes_right, rect.bottom()));
        // The panes mirror the caret while they draw. A click is applied
        // after that, so both panes end the frame on the heading.
        if panes.width() > 1.0 {
            ui.scope_builder(UiBuilder::new().max_rect(panes), |ui| self.panes(ui));
        }
        if outline_w > 1.0 {
            let divider = Rect::from_min_max(
                pos2(panes_right, rect.top()),
                pos2(panes_right + gap, rect.bottom()),
            );
            let outline_rect = Rect::from_min_max(pos2(divider.right(), rect.top()), rect.max);
            let handle_resp =
                ui.interact(divider, egui::Id::new("outline_split"), egui::Sense::drag());
            if handle_resp.hovered() || handle_resp.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            if handle_resp.drag_started() {
                // Start from the width on screen. The stored width can be
                // larger when the window has shrunk the outline to fit.
                self.outline_drag_from = Some(outline_w);
            }
            if handle_resp.dragged()
                && let Some(origin) = self.outline_drag_from
                && let Some(total) = handle_resp.total_drag_delta()
            {
                // The whole movement since the press, so dragging past the
                // minimum and back keeps the edge with the pointer.
                self.layout.drag_outline(origin, total.x);
            }
            if handle_resp.drag_stopped() {
                self.outline_drag_from = None;
                self.remember_layout();
            }
            let colors = theme::current(ui.ctx());
            ui.painter().rect_filled(divider, 0.0, colors.background);
            ui.painter().vline(
                divider.center().x,
                divider.y_range(),
                Stroke::new(1.0, colors.divider),
            );
            self.outline
                .refresh(self.parse.revision(), &self.doc, self.parse.output());
            let caret = self.selection().head;
            let clicked = ui
                .scope_builder(UiBuilder::new().max_rect(outline_rect), |ui| {
                    outline::show(ui, self.outline.rows(), caret)
                })
                .inner;
            if let Some(offset) = clicked {
                let here = self.here();
                self.jump_to(offset);
                self.remember(here);
                self.focus_pane(ui.ctx(), self.focus);
            }
        }
    }

    /// A folder-search row. The same file selects the match; another file
    /// opens, and the match is selected once that file is parsed.
    fn open_search_hit(&mut self, hit: SearchOpen) {
        match hit {
            SearchOpen::File(path) => {
                if self.doc.path() != Some(path.as_path()) {
                    self.request_open(path);
                }
            }
            SearchOpen::Match { path, range, text } => {
                if self.doc.path() == Some(path.as_path()) {
                    let Some(range) = self.search.locate(&self.doc, range, &text) else {
                        return;
                    };
                    let here = self.here();
                    self.show_match(Selection {
                        anchor: range.start,
                        head: range.end,
                    });
                    self.remember(here);
                } else {
                    self.pending_jump = Some((path.clone(), Jump::Select { range, text }));
                    self.request_open(path);
                }
            }
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
        if output.new_folder {
            self.begin_new_folder();
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
                        self.remember(here);
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
            links::Target::Document(path) => {
                self.open_external(path.to_string_lossy().into_owned());
            }
            links::Target::Anchor(anchor) => {
                if self.jump_to_anchor(&anchor) {
                    self.remember(here);
                }
            }
            links::Target::File { path, anchor } if Some(path.as_path()) == self.doc.path() => {
                if anchor.is_none_or(|a| self.jump_to_anchor(&a)) {
                    self.remember(here);
                }
            }
            links::Target::File { path, .. } if !path.exists() => {
                self.show_hint(format!("{} doesn't exist", display_name(&path)));
            }
            links::Target::File { path, anchor } => {
                self.pending_jump = anchor.map(|a| (path.clone(), Jump::Anchor(a)));
                self.pending_history = Some((path.clone(), here, Travel::Follow));
                self.request_open(path);
            }
            links::Target::Refused(reason) => self.show_hint(reason),
        }
    }

    /// Where the caret is now, as a place to come back to.
    fn here(&self) -> Place {
        Place {
            path: self.doc.path().map(Path::to_path_buf),
            offset: self.selection().head,
        }
    }

    /// Records `here` before following a link: a new path forward, so the
    /// places gone back from are dropped.
    fn remember(&mut self, here: Place) {
        self.back.push(here);
        self.forward.clear();
    }

    /// Alt+Left: back to where the last followed link was clicked.
    fn go_back(&mut self) {
        if let Some(place) = self.back.pop() {
            self.travel(Travel::Back(place));
        }
    }

    /// Alt+Right: forward again to where Alt+Left came from.
    fn go_forward(&mut self) {
        if let Some(place) = self.forward.pop() {
            self.travel(Travel::Forward(place));
        }
    }

    /// Goes to a place from the history. The history changes only once the
    /// place is reached: in this file, now; in another, once it's open.
    fn travel(&mut self, travel: Travel) {
        let here = self.here();
        let (Travel::Back(place) | Travel::Forward(place)) = travel.clone() else {
            return;
        };
        match &place.path {
            // Gone since (deleted, or moved by another program): say so,
            // and keep the place, rather than open an empty file there.
            Some(path) if Some(path.as_path()) != self.doc.path() && !path.exists() => {
                self.show_hint(format!("{} no longer exists", display_name(path)));
                self.unwind(travel);
            }
            Some(path) if Some(path.as_path()) != self.doc.path() => {
                self.pending_jump = Some((path.clone(), Jump::Offset(place.offset)));
                self.pending_history = Some((path.clone(), here, travel));
                self.request_open(path.clone());
            }
            _ => {
                self.commit(here, travel);
                self.jump_to(place.offset.min(self.doc.len()));
            }
        }
    }

    /// The history after reaching a place, coming from `here`.
    fn commit(&mut self, here: Place, travel: Travel) {
        match travel {
            Travel::Follow => self.remember(here),
            Travel::Back(_) => self.forward.push(here),
            Travel::Forward(_) => self.back.push(here),
        }
    }

    /// The place didn't get reached: it goes back on its stack.
    fn unwind(&mut self, travel: Travel) {
        match travel {
            Travel::Follow => {}
            Travel::Back(place) => self.back.push(place),
            Travel::Forward(place) => self.forward.push(place),
        }
    }

    /// Settles a history change waiting on an open: done once that file is
    /// the open one; undone once nothing's pending and it isn't (the
    /// unsaved-changes prompt was cancelled, or the open failed).
    fn settle_history(&mut self) {
        let Some((path, here, travel)) = self.pending_history.clone() else {
            return;
        };
        if self.doc.path() == Some(path.as_path()) {
            self.pending_history = None;
            self.commit(here, travel);
        } else if self.confirm.is_none() {
            self.pending_history = None;
            self.unwind(travel);
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

    /// Puts the caret at `offset`, scrolled into view. Both panes: a
    /// Ctrl+click lands here before its release moves focus to the clicked
    /// pane, whose caret is then mirrored to the other.
    fn jump_to(&mut self, offset: usize) {
        let caret = inkmark_buffer::Selection::caret(offset);
        self.code.set_selection(caret);
        self.live.set_selection(caret);
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
            Jump::Select { range, text } => {
                if let Some(range) = self.search.locate(&self.doc, range, &text) {
                    self.show_match(Selection {
                        anchor: range.start,
                        head: range.end,
                    });
                }
            }
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
        self.settle_history();
        self.apply_pending_jump();
        if let Some((_, shown)) = &self.hint {
            let shown = *shown;
            if let Some(left) = os_theme::time_left(shown, HINT_TIME) {
                ctx.request_repaint_after(left);
            } else {
                self.hint = None;
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
                let fraction = self.layout.split_fraction(rect.width());
                let mid = layout::split_mid(rect.left(), rect.width(), fraction);
                let half = layout::SPLIT_GAP / 2.0;
                let left = Rect::from_min_max(rect.min, pos2(mid - half, rect.bottom()));
                let right = Rect::from_min_max(pos2(mid + half, rect.top()), rect.max);
                ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
                    self.code.show(ui, &mut self.doc, Some(&mut self.parse));
                });
                ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
                    self.live.show(ui, &mut self.doc, Some(&mut self.parse));
                });
                let hit_half = layout::SPLIT_HIT / 2.0;
                let hit = Rect::from_min_max(
                    pos2(mid - hit_half, rect.top()),
                    pos2(mid + hit_half, rect.bottom()),
                );
                let handle = ui.interact(hit, egui::Id::new("pane_split"), egui::Sense::drag());
                if handle.hovered() || handle.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if handle.drag_started() {
                    self.split_drag_from = Some(mid);
                }
                if handle.dragged()
                    && let Some(origin) = self.split_drag_from
                    && let Some(total) = handle.total_drag_delta()
                {
                    // Move the line by how far the pointer has traveled, so
                    // a grab off its center does not jump. The fraction is
                    // of the area beside the gap. A narrow window draws a
                    // closer split and leaves this value stored.
                    self.layout
                        .drag_split(rect.left(), rect.width(), origin, total.x);
                }
                if handle.drag_stopped() {
                    self.split_drag_from = None;
                    self.remember_layout();
                }
                ui.painter().vline(
                    mid,
                    rect.y_range(),
                    Stroke::new(2.0, theme::current(ui.ctx()).divider),
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
        self.frame(ui);
    }
}

impl App {
    /// One frame of the whole app. Separate from `eframe::App::ui` so tests
    /// drive exactly this, without a real window.
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.os_theme.poll(&ctx);
        self.note_session(&ctx);
        self.handle_shortcuts(&ctx);
        self.poll_dialog(&ctx);
        self.check_disk(&ctx);
        self.guard_close(&ctx);
        // Dialogs take the keyboard before the panes see it.
        self.recent_ui(&ctx);
        self.keys_ui(&ctx);
        self.hold_focus_for_dialogs(&ctx);

        if self.banner.is_some() || self.error.is_some() {
            egui::Panel::top("banner").show(ui, |ui| self.banner_ui(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_ui(ui));
        if self.find.is_open() {
            self.find_history_keys(&ctx);
            let selection = self.selection();
            let modal = self.modal_open();
            let step = {
                let find = &mut self.find;
                let doc = &mut self.doc;
                egui::Panel::bottom("find")
                    .show(ui, |ui| find.show(ui, doc, selection, modal))
                    .inner
            };
            if step.closed {
                self.focus_pane(&ctx, self.focus);
            }
            if let Some(sel) = step.selection {
                self.show_match(sel);
            }
        }
        if self.goto.is_open() {
            let line_count = self.doc.line_count();
            let modal = self.modal_open();
            let step = {
                let goto = &mut self.goto;
                egui::Panel::bottom("goto")
                    .show(ui, |ui| goto.show(ui, line_count, modal))
                    .inner
            };
            if let Some(line) = step.line {
                let here = self.here();
                let offset = self.doc.line_to_byte(line);
                self.jump_to(offset);
                self.remember(here);
                self.focus_pane(&ctx, self.focus);
            } else if step.closed {
                self.focus_pane(&ctx, self.focus);
            }
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.editor(ui));
        // After the sidebar, so New file opens the prompt on the click's frame.
        self.new_file_ui(&ctx);
        self.new_folder_ui(&ctx);
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

/// The action's description, and its chord when one is bound.
fn binding_tip(keys: &keys::KeyMap, action: Action) -> String {
    let chord = keys.shortcut_text(action);
    if chord.is_empty() {
        action.description().to_owned()
    } else {
        format!("{} ({chord})", action.description())
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
/// The fixed text of a name prompt.
struct PromptText<'a> {
    id: &'a str,
    heading: &'a str,
    /// The folder or path it applies to.
    detail: &'a str,
    /// The confirming button.
    action: &'a str,
}

fn name_modal(
    ctx: &egui::Context,
    text: PromptText<'_>,
    name: &mut String,
    error: Option<&str>,
    select: Option<usize>,
) -> (bool, bool) {
    let PromptText {
        id,
        heading,
        detail,
        action,
    } = text;
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
        let mut edit = egui::TextEdit::singleline(name)
            .id(field)
            .desired_width(f32::INFINITY)
            .show(ui);
        if let Some(n) = select {
            use egui::text::{CCursor, CCursorRange};
            edit.state
                .cursor
                .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(n))));
            edit.state.store(ui.ctx(), field);
        }
        if let Some(error) = error {
            ui.label(RichText::new(error).color(theme::current(ui.ctx()).error));
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

    /// Advance width and atlas size of one proportional glyph.
    fn glyph_box(ctx: &egui::Context, c: char) -> (f32, f32, f32) {
        let font = egui::FontId::proportional(13.0);
        ctx.fonts_mut(|fonts| {
            let galley = fonts.layout_no_wrap(c.to_string(), font, egui::Color32::WHITE);
            let glyph = galley.rows[0].glyphs[0];
            (
                glyph.advance_width,
                glyph.uv_rect.size.x,
                glyph.uv_rect.size.y,
            )
        })
    }

    #[test]
    fn the_chrome_draws_the_folder_and_unsaved_marks() {
        let ctx = egui::Context::default();
        // Default proportional faces replace all three marks with one box.
        let mut first = ctx.run_ui(egui::RawInput::default(), |_| {});
        first.textures_delta.clear();
        let missing = glyph_box(&ctx, '▸');
        assert_eq!(glyph_box(&ctx, '▾').1, missing.1);
        assert_eq!(glyph_box(&ctx, '▾').2, missing.2);
        assert_eq!(glyph_box(&ctx, '●').1, missing.1);
        assert_eq!(glyph_box(&ctx, '●').2, missing.2);

        install_ui_font(&ctx);
        let mut second = ctx.run_ui(egui::RawInput::default(), |_| {});
        second.textures_delta.clear();
        for mark in ['▸', '▾', '●', '«', '»', '↑', '↗', '↻', '⊞', '∗'] {
            let drawn = glyph_box(&ctx, mark);
            assert_ne!(
                (drawn.1, drawn.2),
                (missing.1, missing.2),
                "{mark} still uses the missing-glyph box"
            );
        }
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
    fn reopening_restores_panes_and_minimaps() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("recent");
        let layout_path = dir.path().join("layout");
        let ctx = egui::Context::default();
        let mut app = App::with_recent(&ctx, None, recent::Recent::from_store(Some(store.clone())));
        let mut time = 0.0;
        drive(&ctx, &mut app, &mut time, vec![]);
        assert!(!layout_path.exists(), "a frame wrote the layout");
        assert!(app.mode == Mode::Split);
        assert!(app.code.show_minimap && app.live.show_minimap);

        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::E, egui::Modifiers::COMMAND)],
        );
        assert!(app.mode == Mode::Code && app.focus == Pane::Code);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::M, egui::Modifiers::COMMAND)],
        );
        assert!(!app.code.show_minimap);
        assert!(app.live.show_minimap);
        let stored = fs::read_to_string(&layout_path).unwrap();
        assert!(stored.starts_with("code\n0\n1\n"), "{stored}");

        let ctx = egui::Context::default();
        let mut app = App::with_recent(&ctx, None, recent::Recent::from_store(Some(store.clone())));
        assert!(app.mode == Mode::Code && app.focus == Pane::Code);
        assert!(!app.code.show_minimap);
        assert!(app.live.show_minimap);

        // Ctrl+2 leaves code-only and shows the live pane.
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Num2, egui::Modifiers::COMMAND)],
        );
        assert!(app.mode == Mode::Live && app.focus == Pane::Live);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::M, egui::Modifiers::COMMAND)],
        );
        assert!(!app.code.show_minimap && !app.live.show_minimap);

        let ctx = egui::Context::default();
        let app = App::with_recent(&ctx, None, recent::Recent::from_store(Some(store)));
        assert!(app.mode == Mode::Live && app.focus == Pane::Live);
        assert!(!app.code.show_minimap && !app.live.show_minimap);
    }

    #[test]
    fn reopening_restores_the_split_and_the_outline() {
        let dir = tempfile::tempdir().unwrap();
        let layout_path = dir.path().join("layout");
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        let mut time = 0.0;
        // The divider has to be drawn once before a press can grab it.
        drive(&ctx, &mut app, &mut time, vec![]);
        let panes_left = app.sidebar.width + outline::GAP;
        let panes_right = 1000.0 - outline::PREFERRED_WIDTH - outline::GAP;
        let width = panes_right - panes_left;
        let mid = layout::split_mid(panes_left, width, app.layout.split_fraction(width));
        let start = egui::pos2(mid, 80.0);
        let end = egui::pos2(mid + 80.0, 80.0);
        let down = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::PointerMoved(start), down(start, true)],
        );
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::PointerMoved(end)],
        );
        assert!(
            app.layout.code_fraction > 0.5,
            "split stayed {}",
            app.layout.code_fraction
        );
        assert!(
            !layout_path.exists(),
            "the split was written during the drag"
        );
        drive(&ctx, &mut app, &mut time, vec![down(end, false)]);
        let fraction = app.layout.code_fraction;
        let after_split = fs::read_to_string(&layout_path).unwrap();
        assert!(after_split.starts_with("split\n1\n1\n"), "{after_split}");

        let outline_x = panes_right + outline::GAP / 2.0;
        let outline_start = egui::pos2(outline_x, 80.0);
        let outline_end = egui::pos2(outline_x + 40.0, 80.0);
        // Arrive at the handle before the press, so the jump from the split
        // is not part of this drag.
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::PointerMoved(outline_start)],
        );
        drive(&ctx, &mut app, &mut time, vec![down(outline_start, true)]);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::PointerMoved(outline_end)],
        );
        assert!(
            app.layout.outline_width < outline::PREFERRED_WIDTH,
            "outline grew to {}",
            app.layout.outline_width
        );
        let during = fs::read_to_string(&layout_path).unwrap();
        assert_eq!(
            during, after_split,
            "the outline was written during the drag"
        );
        drive(&ctx, &mut app, &mut time, vec![down(outline_end, false)]);
        let outline_width = app.layout.outline_width;
        assert!(outline_width > outline::MIN_WIDTH, "{outline_width}");
        let (_, drawn) =
            outline::column_widths(1000.0, Some(app.sidebar.width), Some(outline_width));
        assert_eq!(drawn, outline_width);

        let ctx = egui::Context::default();
        let reloaded = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert_eq!(reloaded.layout.code_fraction, fraction);
        assert_eq!(reloaded.layout.outline_width, outline_width);
        assert!(reloaded.mode == Mode::Split);
        assert!(reloaded.code.show_minimap && reloaded.live.show_minimap);
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
        assert_eq!(app.rename.as_ref().unwrap().name, "draft.md");
        let mut time = 0.0;
        // The stem is selected: typing replaces it and keeps `.md`.
        drive(&ctx, &mut app, &mut time, vec![]);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::Text("final".into())],
        );
        assert_eq!(app.rename.as_ref().unwrap().name, "final.md");
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
        app.rename.as_mut().unwrap().name = "b.md".into();
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

    #[test]
    fn confirming_a_rename_unchanged_keeps_a_dotted_name() {
        // Review of #24: `my.notes.md` used to lose its `.md`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2024.01.02.md");
        fs::write(&path, "x\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(path.clone()));
        let mut time = 0.0;
        app.begin_rename(path.clone());
        drive(&ctx, &mut app, &mut time, vec![]);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert!(app.rename.is_none());
        assert!(path.exists());
        assert_eq!(app.doc.path(), Some(path.as_path()));
        // Editing the selected stem keeps the extension too.
        app.begin_rename(path.clone());
        drive(&ctx, &mut app, &mut time, vec![]);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::Text("2024.01.03".into())],
        );
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let renamed = dir.path().join("2024.01.03.md");
        assert!(renamed.exists());
        assert_eq!(app.doc.path(), Some(renamed.as_path()));
    }

    #[test]
    fn back_follows_a_rename_and_never_opens_a_missing_file() {
        // Review of #24: Back to the old name used to open an empty new
        // file there, dropping the note.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "Claim[^1].\n\n[^1]: Source.\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        app.jump_to(2);
        app.follow(offset_of(&app, "[^1]"));
        let b = dir.path().join("b.md");
        app.begin_rename(a.clone());
        app.rename.as_mut().unwrap().name = "b.md".into();
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert_eq!(app.doc.path(), Some(b.as_path()));
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::ArrowLeft, egui::Modifiers::ALT)],
        );
        assert_eq!(app.doc.path(), Some(b.as_path()));
        assert_eq!(app.selection().head, 2);
        assert_eq!(text(&app), "Claim[^1].\n\n[^1]: Source.\n");

        // A place in a file deleted since: say so, stay put.
        let gone = dir.path().join("gone.md");
        app.back.push(Place {
            path: Some(gone.clone()),
            offset: 0,
        });
        app.go_back();
        assert_eq!(app.doc.path(), Some(b.as_path()));
        assert!(app.hint.as_ref().unwrap().0.contains("gone.md"));
        assert!(!gone.exists());
    }

    #[test]
    fn a_link_to_a_missing_note_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "[todo](later.md)\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        app.follow(1);
        assert_eq!(app.doc.path(), Some(a.as_path()));
        assert!(app.hint.as_ref().unwrap().0.contains("later.md"));
        assert!(app.back.is_empty());
    }

    #[test]
    fn ctrl_click_in_the_live_pane_jumps_while_the_code_pane_has_focus() {
        // Review of #24: the jump ran before the click's release moved
        // focus, and mirroring the clicked pane's old caret undid it.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "Claim[^1] here.\n\nMore.\n\n[^1]: The source.\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a));
        app.sidebar.visible = false;
        assert!(app.mode == Mode::Split && app.focus == Pane::Code);
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::ModifiersChanged(egui::Modifiers::COMMAND)],
        );
        // Find the reference in the live pane (right half of the panes, with
        // the outline beyond it) by the hand cursor Ctrl shows over links.
        let frame_cursor = |app: &mut App, time: &mut f64, pos: egui::Pos2| {
            *time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_max(
                    egui::Pos2::ZERO,
                    egui::pos2(1000.0, 700.0),
                )),
                time: Some(*time),
                events: vec![egui::Event::PointerMoved(pos)],
                ..Default::default()
            };
            let out = ctx.run_ui(input, |ui| app.editor(ui));
            out.platform_output.cursor_icon
        };
        let y = 30.0;
        let (_, outline_w) = outline::column_widths(1000.0, None, Some(outline::PREFERRED_WIDTH));
        let panes_right = 1000.0 - outline_w - outline::GAP;
        let live_left = (panes_right / 2.0) as i32 + 16;
        let live_right = panes_right as i32 - 8;
        let mut found = None;
        for x in (live_left..live_right).step_by(3) {
            for y in [y - 20.0, y - 10.0, y, y + 10.0] {
                let pos = egui::pos2(x as f32, y);
                if frame_cursor(&mut app, &mut time, pos) == egui::CursorIcon::PointingHand {
                    found = Some(pos);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let pos = found.expect("the reference shows a hand under Ctrl");
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::COMMAND,
        };
        drive(&ctx, &mut app, &mut time, vec![button(true)]);
        drive(&ctx, &mut app, &mut time, vec![button(false)]);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)],
        );
        let note = offset_of(&app, "The source");
        assert_eq!(app.live.selection().head, note);
        assert_eq!(app.code.selection().head, note);
    }

    /// Runs whole-app frames (`App::frame`, as the window does) and keeps
    /// what each frame drew and asked the window to do.
    struct Run {
        ctx: egui::Context,
        app: App,
        time: f64,
        out: egui::FullOutput,
    }

    impl Run {
        fn new(dir: &Path, path: Option<PathBuf>) -> Self {
            let ctx = egui::Context::default();
            let recent = recent::Recent::from_store(Some(dir.join("recent")));
            let app = App::with_recent(&ctx, path, recent);
            let mut run = Self {
                ctx,
                app,
                time: 0.0,
                out: Default::default(),
            };
            run.settle();
            run
        }

        fn input(&mut self, events: Vec<egui::Event>) -> egui::RawInput {
            self.time += 1.0 / 60.0;
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_max(
                    egui::Pos2::ZERO,
                    egui::pos2(1000.0, 700.0),
                )),
                time: Some(self.time),
                events,
                ..Default::default()
            }
        }

        fn run(&mut self, input: egui::RawInput) {
            let app = &mut self.app;
            self.out = self.ctx.run_ui(input, |ui| app.frame(ui));
            self.out.textures_delta.clear();
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = self.input(events);
            self.run(input);
        }

        fn key(&mut self, key: egui::Key, modifiers: egui::Modifiers) {
            self.frame(vec![shortcut(key, modifiers)]);
        }

        /// The window's close button.
        fn close_window(&mut self) {
            let mut input = self.input(vec![]);
            input.viewports.insert(
                egui::ViewportId::ROOT,
                egui::ViewportInfo {
                    events: vec![egui::ViewportEvent::Close],
                    ..Default::default()
                },
            );
            self.run(input);
        }

        fn sent(&self, command: &ViewportCommand) -> bool {
            self.out
                .viewport_output
                .get(&egui::ViewportId::ROOT)
                .is_some_and(|v| v.commands.contains(command))
        }

        /// Where the last frame drew `text` (a button or a list row).
        fn text_rect(&self, text: &str) -> Option<egui::Rect> {
            fn find(shape: &egui::Shape, text: &str) -> Option<egui::Rect> {
                match shape {
                    egui::Shape::Text(t) if t.galley.text() == text => {
                        Some(t.galley.rect.translate(t.pos.to_vec2()))
                    }
                    egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| find(s, text)),
                    _ => None,
                }
            }
            // The last match is drawn on top (a dialog over the sidebar).
            self.out
                .shapes
                .iter()
                .rev()
                .find_map(|c| find(&c.shape, text))
        }

        fn click_text(&mut self, text: &str) {
            // A dialog's first frame only measures it; give it a frame or
            // two to be drawn.
            for _ in 0..3 {
                if self.text_rect(text).is_some() {
                    break;
                }
                self.frame(vec![]);
            }
            let at = self
                .text_rect(text)
                .unwrap_or_else(|| panic!("{text:?} isn't on screen"))
                .center();
            let button = |pressed| egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(at)]);
            self.frame(vec![button(true)]);
            self.frame(vec![button(false)]);
            self.frame(vec![]);
        }

        /// Every place the last frame drew `text`.
        fn text_rects(&self, text: &str) -> Vec<egui::Rect> {
            fn walk(shape: &egui::Shape, text: &str, out: &mut Vec<egui::Rect>) {
                match shape {
                    egui::Shape::Text(t) if t.galley.text() == text => {
                        out.push(t.galley.rect.translate(t.pos.to_vec2()));
                    }
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            walk(shape, text, out);
                        }
                    }
                    _ => {}
                }
            }
            let mut out = Vec::new();
            for clip in &self.out.shapes {
                walk(&clip.shape, text, &mut out);
            }
            out
        }

        /// Clicks the topmost `mark` in the code pane. The sidebar draws the
        /// same ▾ and ▸ further left.
        fn click_code_mark(&mut self, mark: &str) {
            let in_code = |rect: &egui::Rect| rect.left() > 220.0 && rect.left() < 520.0;
            for _ in 0..3 {
                if self.text_rects(mark).iter().any(in_code) {
                    break;
                }
                self.frame(vec![]);
            }
            let at = self
                .text_rects(mark)
                .into_iter()
                .filter(in_code)
                .min_by(|a, b| a.top().total_cmp(&b.top()))
                .unwrap_or_else(|| panic!("{mark} isn't in the code pane"))
                .center();
            let button = |pressed| egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(at)]);
            self.frame(vec![button(true)]);
            self.frame(vec![button(false)]);
            self.frame(vec![]);
        }

        fn settle(&mut self) {
            let start = Instant::now();
            while !self.app.parse.is_settled() {
                self.frame(vec![]);
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(2));
            }
            self.frame(vec![]);
        }

        fn wait_search(&mut self) {
            let start = Instant::now();
            while !self.app.search.settled() {
                self.frame(vec![]);
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "folder search did not finish"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            self.frame(vec![]);
        }

        fn check_disk_now(&mut self) {
            self.app.next_disk_check = Instant::now();
            self.frame(vec![]);
        }
    }

    #[test]
    fn find_selects_a_match_and_replace_all_is_one_undo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "one cat\ntwo cat\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.key(egui::Key::F, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("cat".into())]);
        run.frame(vec![]);
        assert_eq!(text(&run.app), "one cat\ntwo cat\n");
        assert_eq!(run.app.code.selection().range(), 4..7);
        assert_eq!(run.app.find.match_count(), 2);
        assert_eq!(run.app.find.match_index(), Some(1));

        run.app.find.set_replacement("dog");
        run.click_text("Replace all");
        assert_eq!(text(&run.app), "one dog\ntwo dog\n");
        run.key(egui::Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(text(&run.app), "one cat\ntwo cat\n");
        assert!(!run.app.doc.can_undo());
    }

    #[test]
    fn a_filename_query_opens_that_note() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.md"), "hello\n").unwrap();
        fs::write(dir.path().join("beta.md"), "token\n").unwrap();
        let mut run = Run::new(dir.path(), Some(dir.path().to_path_buf()));
        run.app.sidebar.visible = false;
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        assert!(run.app.sidebar.visible);
        assert!(run.app.search.is_open());
        run.frame(vec![egui::Event::Text("alpha".into())]);
        run.wait_search();
        run.click_text("alpha.md");
        assert_eq!(
            run.app
                .doc
                .path()
                .and_then(|path| path.file_name().map(|n| n.to_owned())),
            Some(std::ffi::OsString::from("alpha.md"))
        );
        assert_eq!(text(&run.app), "hello\n");
    }

    #[test]
    fn a_content_query_opens_the_match() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha.md");
        fs::write(&alpha, "hello\n").unwrap();
        fs::write(dir.path().join("beta.md"), "see token here\n").unwrap();
        let mut run = Run::new(dir.path(), Some(alpha));
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("token".into())]);
        run.wait_search();
        run.click_text("beta.md  see token here");
        run.settle();
        assert_eq!(
            run.app
                .doc
                .path()
                .and_then(|path| path.file_name().map(|n| n.to_owned())),
            Some(std::ffi::OsString::from("beta.md"))
        );
        assert_eq!(
            run.app.doc.slice(run.app.code.selection().range()).as_ref(),
            "token"
        );
    }

    #[test]
    fn a_match_in_an_edited_file_selects_the_moved_text() {
        let dir = tempfile::tempdir().unwrap();
        let beta = dir.path().join("beta.md");
        fs::write(&beta, "see token here\n").unwrap();
        let mut run = Run::new(dir.path(), Some(beta));
        run.app
            .doc
            .apply(
                vec![inkmark_buffer::Edit::insert(0, "xx")],
                Selection::caret(0),
                Selection::caret(2),
                inkmark_buffer::EditKind::Other,
            )
            .unwrap();
        assert!(run.app.doc.is_dirty());
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("token".into())]);
        run.wait_search();
        run.click_text("beta.md  see token here");
        assert_eq!(
            run.app.doc.slice(run.app.code.selection().range()).as_ref(),
            "token"
        );
        assert_eq!(run.app.code.selection().range(), 6..11);
    }

    #[test]
    fn a_changed_file_selects_the_match_text() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha.md");
        fs::write(&alpha, "hello\n").unwrap();
        let beta = dir.path().join("beta.md");
        fs::write(&beta, "see token here\n").unwrap();
        let mut run = Run::new(dir.path(), Some(alpha));
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("token".into())]);
        run.wait_search();
        fs::write(&beta, "xxsee token here\n").unwrap();
        run.click_text("beta.md  see token here");
        run.settle();
        assert_eq!(
            run.app.doc.slice(run.app.code.selection().range()).as_ref(),
            "token"
        );
        assert_eq!(run.app.code.selection().range(), 6..11);
    }

    #[test]
    fn reopening_search_lists_a_note_added_while_it_was_closed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.md"), "hello\n").unwrap();
        let mut run = Run::new(dir.path(), Some(dir.path().to_path_buf()));
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("gamma".into())]);
        run.wait_search();
        assert!(run.text_rect("gamma.md").is_none());
        fs::write(dir.path().join("gamma.md"), "x\n").unwrap();
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.search.is_open());
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        assert!(run.app.search.is_open());
        run.wait_search();
        assert!(run.text_rect("gamma.md").is_some());
    }

    #[test]
    fn an_invalid_folder_pattern_stays_in_the_sidebar() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("alpha.md"), "hello\n").unwrap();
        let mut run = Run::new(dir.path(), Some(dir.path().to_path_buf()));
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("(".into())]);
        run.click_text("Regex");
        assert!(run.app.search.error().is_some());
        assert!(run.text_rect("New file").is_none());
    }

    #[test]
    fn find_next_click_moves_on_the_frame_the_query_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "one cat\ntwo cat\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.key(egui::Key::F, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("c".into())]);
        run.frame(vec![]);
        let at = run.text_rect("Next").expect("Next").center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        // The query becomes "cat" and Next is clicked in this frame.
        run.frame(vec![
            egui::Event::PointerMoved(at),
            egui::Event::Text("at".into()),
            button(true),
            button(false),
        ]);
        run.frame(vec![]);
        assert_eq!(run.app.code.selection().range(), 12..15);
        assert_eq!(run.app.find.match_index(), Some(2));
    }

    #[test]
    fn closing_with_unsaved_changes_asks_and_cancel_keeps_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path.clone()));
        type_into(&mut r.app, "two\n");
        r.close_window();
        assert_eq!(r.app.confirm, Some(Confirm::Close));
        assert!(
            r.sent(&ViewportCommand::CancelClose),
            "the close is held back"
        );
        r.frame(vec![]);
        r.click_text("Cancel");
        assert!(r.app.confirm.is_none());
        r.frame(vec![]);
        assert!(!r.sent(&ViewportCommand::Close));
        assert_eq!(text(&r.app), "one\ntwo\n");
        // Escape cancels too.
        r.close_window();
        r.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(r.app.confirm.is_none());
        assert!(!r.sent(&ViewportCommand::Close));
    }

    #[test]
    fn closing_discards_or_saves_as_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();

        let mut r = Run::new(dir.path(), Some(path.clone()));
        type_into(&mut r.app, "two\n");
        r.close_window();
        r.frame(vec![]);
        r.click_text("Discard");
        assert!(r.sent(&ViewportCommand::Close));
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\n", "nothing saved");

        let mut r = Run::new(dir.path(), Some(path.clone()));
        type_into(&mut r.app, "two\n");
        r.close_window();
        r.frame(vec![]);
        r.click_text("Save");
        r.frame(vec![]);
        assert!(r.sent(&ViewportCommand::Close));
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn closing_a_saved_document_doesnt_ask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path));
        r.close_window();
        assert!(r.app.confirm.is_none());
        assert!(!r.sent(&ViewportCommand::CancelClose));
    }

    #[test]
    fn opening_another_file_saves_discards_or_cancels_as_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        for (choice, saved, opened) in [
            ("Save", true, true),
            ("Discard", false, true),
            ("Cancel", false, false),
        ] {
            fs::write(&a, "a\n").unwrap();
            let mut r = Run::new(dir.path(), Some(a.clone()));
            type_into(&mut r.app, "edit\n");
            r.app.request_open(b.clone());
            r.frame(vec![]);
            r.click_text(choice);
            assert!(r.app.confirm.is_none(), "{choice}");
            assert_eq!(
                fs::read_to_string(&a).unwrap() == "a\nedit\n",
                saved,
                "{choice}: saved?"
            );
            let now = r.app.doc.path().unwrap().to_path_buf();
            assert_eq!(now == b, opened, "{choice}: opened?");
            if !opened {
                assert_eq!(text(&r.app), "a\nedit\n", "{choice}: edits kept");
            }
        }
    }

    #[test]
    fn keep_mine_and_reload_answer_a_change_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path.clone()));
        type_into(&mut r.app, "mine\n");
        fs::write(&path, "theirs\n").unwrap();
        r.check_disk_now();
        assert!(matches!(r.app.banner, Some(Banner::DiskChanged)));
        r.click_text("Keep mine");
        assert!(r.app.banner.is_none());
        r.check_disk_now();
        assert!(r.app.banner.is_none(), "their version counts as seen");
        // Saving now writes ours over theirs, as chosen.
        r.app.save();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\nmine\n");

        fs::write(&path, "theirs again\n").unwrap();
        r.check_disk_now();
        r.click_text("Reload");
        assert_eq!(text(&r.app), "theirs again\n");
        assert!(r.app.banner.is_none());
    }

    #[test]
    fn a_deleted_file_is_noticed_and_the_banner_dismissed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path.clone()));
        fs::remove_file(&path).unwrap();
        r.check_disk_now();
        assert!(matches!(r.app.banner, Some(Banner::DiskMissing)));
        r.click_text("Dismiss");
        assert!(r.app.banner.is_none());
        r.check_disk_now();
        assert!(r.app.banner.is_none(), "dismissed stays dismissed");
        // If a file appears there again, that's a change.
        fs::write(&path, "new\n").unwrap();
        r.check_disk_now();
        assert!(matches!(r.app.banner, Some(Banner::DiskChanged)));
        assert_eq!(text(&r.app), "one\n");
    }

    #[test]
    fn the_recent_list_opens_by_keyboard_or_click() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a.md"),
            dir.path().join("b.md"),
            dir.path().join("c.md"),
        );
        for p in [&a, &b, &c] {
            fs::write(p, "x\n").unwrap();
        }
        let mut r = Run::new(dir.path(), Some(a.clone()));
        for p in [&b, &c] {
            r.app.recent.add(p);
        }
        // c, b, a: Ctrl+R, Down, Enter opens b.
        r.key(egui::Key::R, egui::Modifiers::COMMAND);
        assert_eq!(r.app.recent_list, Some(0));
        r.key(egui::Key::ArrowDown, egui::Modifiers::NONE);
        r.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert!(r.app.recent_list.is_none());
        assert_eq!(r.app.doc.path(), Some(b.as_path()));
        // Escape closes it; a click on a row opens that file.
        r.key(egui::Key::R, egui::Modifiers::COMMAND);
        r.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(r.app.recent_list.is_none());
        r.key(egui::Key::R, egui::Modifiers::COMMAND);
        r.frame(vec![]);
        r.click_text("c.md");
        assert_eq!(r.app.doc.path(), Some(c.as_path()));
    }

    #[test]
    fn the_command_line_takes_one_path_or_an_option() {
        let parse = |args: &[&str]| parse_args(args.iter().map(std::ffi::OsString::from));
        assert_eq!(parse(&[]), Ok(Command::Run(None)));
        assert_eq!(
            parse(&["notes.md"]),
            Ok(Command::Run(Some("notes.md".into())))
        );
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["notes.md", "-V"]), Ok(Command::Version));
        assert_eq!(parse(&["--version"]), Ok(Command::Version));
        assert_eq!(parse(&["--list-keys"]), Ok(Command::ListKeys));
        // A file whose name starts with `-`, after `--`; a lone `-` is a name.
        assert_eq!(
            parse(&["--", "--help"]),
            Ok(Command::Run(Some("--help".into())))
        );
        assert_eq!(parse(&["-"]), Ok(Command::Run(Some("-".into()))));
        assert!(
            parse(&["--frobnicate"])
                .unwrap_err()
                .contains("--frobnicate")
        );
        assert!(parse(&["a.md", "b.md"]).is_err());
    }

    #[test]
    fn font_settings_apply_and_report_missing_fonts() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path(), None);
        let config = dir.path().join("config.toml");
        fs::write(
            &config,
            "[font]\ncode = \"No Such Mono 123\"\ntext_size = 20\n",
        )
        .unwrap();
        let asked = std::cell::RefCell::new(Vec::new());
        app.apply_settings_from(Some(&config), |alias| {
            asked.borrow_mut().push(alias.to_owned());
            None
        });
        assert_eq!(app.live.font_size, 20.0);
        assert_eq!(app.code.font_size, CODE_SIZE);
        assert!(
            app.error.as_deref().unwrap().contains("No Such Mono 123"),
            "{:?}",
            app.error
        );
        // The configured code font isn't installed, so it falls back to the
        // system's too.
        assert_eq!(
            *asked.borrow(),
            vec!["monospace".to_owned(), "sans-serif".to_owned()]
        );

        app.error = None;
        fs::write(&config, "[font]\ncode_size = \"big\"\n").unwrap();
        app.apply_settings_from(Some(&config), |_| None);
        assert!(app.error.as_deref().unwrap().contains("code_size"));
        assert_eq!(
            app.live.font_size, 20.0,
            "a bad file keeps the last good settings"
        );
    }

    #[test]
    fn settings_fall_back_scale_and_clear_their_own_errors() {
        // Review of #30.
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path(), None);
        let Some(installed) = ["DejaVu Sans", "Noto Sans", "Liberation Sans"]
            .into_iter()
            .find(|f| app.fonts.borrow().has_family(f))
        else {
            return;
        };
        let system = |alias: &str| (alias == "sans-serif").then(|| installed.to_owned());
        let config = dir.path().join("config.toml");

        // A configured font that isn't installed falls back to the system's.
        fs::write(
            &config,
            "[font]\ntext = \"No Such Sans 123\"\ntext_size = 24\n",
        )
        .unwrap();
        app.apply_settings_from(Some(&config), system);
        assert_eq!(app.fonts.borrow().families().1, installed);
        assert!(
            app.error
                .as_deref()
                .unwrap()
                .contains("using the system font")
        );
        // Line height follows the size.
        assert_eq!(app.live.line_height, (24.0 * TEXT_LINE).round());
        assert_eq!(app.code.line_height, 21.0);

        // Fixed: the settings' own error goes away.
        fs::write(&config, "[font]\ntext_size = 24\n").unwrap();
        app.apply_settings_from(Some(&config), system);
        assert!(app.error.is_none(), "{:?}", app.error);

        // An unrelated error stays.
        app.error = Some("Couldn't save: disk full".into());
        app.apply_settings_from(Some(&config), system);
        assert_eq!(app.error.as_deref(), Some("Couldn't save: disk full"));

        // A file that exists but can't be read keeps the previous settings.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = fs::read_to_string(&config).is_err();
        app.error = None;
        app.apply_settings_from(Some(&config), system);
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();
        if unreadable {
            assert!(
                app.error
                    .as_deref()
                    .unwrap()
                    .contains("keeping the previous")
            );
            assert_eq!(app.live.font_size, 24.0);
        }
        // A deleted file means defaults.
        fs::remove_file(&config).unwrap();
        app.apply_settings_from(Some(&config), system);
        assert_eq!(app.live.font_size, TEXT_SIZE);
        assert!(app.error.is_none());
    }

    #[test]
    fn extra_shift_does_not_fire_a_shorter_app_shortcut() {
        // consume_shortcut treated Ctrl+Shift+M as Ctrl+M, and Ctrl+Shift+1
        // as Ctrl+1. Exact matching leaves both alone.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "word\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path));
        assert!(r.app.code.show_minimap);
        r.key(
            egui::Key::M,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert!(r.app.code.show_minimap);
        r.key(egui::Key::M, egui::Modifiers::COMMAND);
        assert!(!r.app.code.show_minimap);

        r.app.focus_pane(&r.ctx, Pane::Live);
        r.key(
            egui::Key::Num1,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert!(r.app.focus == Pane::Live);
        r.key(egui::Key::Num1, egui::Modifiers::COMMAND);
        assert!(r.app.focus == Pane::Code);
    }

    #[test]
    fn a_pane_chord_on_a_former_app_key_is_not_eaten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "word\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path));
        let mut keys = keys::KeyMap::builtin();
        keys.set(Action::Bold, vec![keys::Chord::parse("Ctrl+E").unwrap()]);
        keys.set(Action::CycleMode, vec![]);
        r.app.keys = keys;
        r.app.install_keys();
        r.app
            .code
            .set_selection(inkmark_buffer::Selection { anchor: 0, head: 4 });
        r.app.code.request_focus(&r.ctx);
        r.key(egui::Key::E, egui::Modifiers::COMMAND);
        assert!(r.app.mode == Mode::Split, "cycle_mode was unbound");
        assert_eq!(text(&r.app), "**word**\n");
    }

    fn select_word(r: &mut Run) {
        let start = text(&r.app).find("word").unwrap();
        r.app.code.set_selection(inkmark_buffer::Selection {
            anchor: start,
            head: start + 4,
        });
        r.app.code.request_focus(&r.ctx);
    }

    #[test]
    fn config_keys_rebind_live_and_a_bad_file_keeps_the_last_good_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "word\n").unwrap();
        let config = dir.path().join("config.toml");
        let mut r = Run::new(dir.path(), Some(path));

        // A chord that doesn't parse is reported; the font still applies
        // and bold stays on Ctrl+B.
        fs::write(&config, "[font]\ntext_size = 20\n[keys]\nbold = \"nope\"\n").unwrap();
        r.app.apply_settings_from(Some(&config), |_| None);
        assert_eq!(r.app.live.font_size, 20.0);
        assert!(
            r.app.error.as_deref().unwrap().contains("keys.bold"),
            "{:?}",
            r.app.error
        );
        select_word(&mut r);
        r.key(egui::Key::B, egui::Modifiers::COMMAND);
        assert_eq!(text(&r.app), "**word**\n");

        // A real rebind replaces it, and an empty list unbinds the mode cycle.
        r.key(egui::Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(text(&r.app), "word\n");
        fs::write(&config, "[keys]\nbold = \"Ctrl+L\"\ncycle_mode = []\n").unwrap();
        r.app.apply_settings_from(Some(&config), |_| None);
        assert!(r.app.error.is_none(), "{:?}", r.app.error);
        select_word(&mut r);
        r.key(egui::Key::B, egui::Modifiers::COMMAND);
        r.key(egui::Key::E, egui::Modifiers::COMMAND);
        assert_eq!(text(&r.app), "word\n");
        assert!(r.app.mode == Mode::Split);
        r.key(egui::Key::L, egui::Modifiers::COMMAND);
        assert_eq!(text(&r.app), "**word**\n");

        // Syntax that doesn't parse keeps those bindings.
        fs::write(&config, "[keys").unwrap();
        r.app.apply_settings_from(Some(&config), |_| None);
        assert!(
            r.app
                .error
                .as_deref()
                .unwrap()
                .contains("keeping the previous")
        );
        select_word(&mut r);
        r.key(egui::Key::L, egui::Modifiers::COMMAND);
        assert_eq!(text(&r.app), "word\n");

        // The watched file is re-read, and unbinding bold takes Ctrl+L away.
        r.app.font_files = vec![Some(config.clone())];
        r.app.font_stamps = vec![Some(std::time::SystemTime::UNIX_EPOCH)];
        fs::write(&config, "[keys]\nbold = []\n").unwrap();
        r.check_disk_now();
        assert!(r.app.error.is_none(), "{:?}", r.app.error);
        select_word(&mut r);
        r.key(egui::Key::L, egui::Modifiers::COMMAND);
        r.key(egui::Key::B, egui::Modifiers::COMMAND);
        assert_eq!(text(&r.app), "word\n");
    }

    #[test]
    fn a_conflicting_key_keeps_the_default_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "word\n").unwrap();
        let config = dir.path().join("config.toml");
        let mut r = Run::new(dir.path(), Some(path));
        // italic takes link's chord, so it snaps back, and bold can only
        // sit on Ctrl+I while italic is away.
        fs::write(&config, "[keys]\nbold = \"Ctrl+I\"\nitalic = \"Ctrl+K\"\n").unwrap();
        r.app.apply_settings_from(Some(&config), |_| None);
        let error = r.app.error.as_deref().unwrap();
        assert!(error.contains("keys.bold"), "{error}");
        assert!(error.contains("keys.italic"), "{error}");
        select_word(&mut r);
        r.key(egui::Key::B, egui::Modifiers::COMMAND);
        assert_eq!(text(&r.app), "**word**\n");
    }

    #[test]
    fn alt_shift_left_moves_a_table_column_instead_of_going_back() {
        // egui matches Alt+Shift+Left as Alt+Left (Back) unless told not to.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.md");
        fs::write(&path, "| a | b |\n|---|---|\n| c | d |\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path));
        r.app.back.push(Place {
            path: None,
            offset: 0,
        });
        r.app.focus = Pane::Live;
        r.app.live.request_focus(&r.ctx);
        r.app.jump_to(text(&r.app).find('d').unwrap());
        r.frame(vec![]);
        r.key(
            egui::Key::ArrowLeft,
            egui::Modifiers::ALT.plus(egui::Modifiers::SHIFT),
        );
        assert_eq!(r.app.back.len(), 1, "Back wasn't taken");
        assert!(text(&r.app).contains("| d   | c   |"), "{}", text(&r.app));
    }

    #[test]
    fn alt_right_goes_forward_again_until_a_new_link_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "Claim[^1] and[^2].\n\n[^1]: One.\n\n[^2]: Two.\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        app.jump_to(2);
        app.follow(offset_of(&app, "[^1]"));
        let note = offset_of(&app, "One.");
        assert_eq!(app.selection().head, note);
        let alt = |key| shortcut(key, egui::Modifiers::ALT);
        drive(&ctx, &mut app, &mut time, vec![alt(egui::Key::ArrowLeft)]);
        assert_eq!(app.selection().head, 2);
        drive(&ctx, &mut app, &mut time, vec![alt(egui::Key::ArrowRight)]);
        assert_eq!(app.selection().head, note, "forward to the note again");
        drive(&ctx, &mut app, &mut time, vec![alt(egui::Key::ArrowLeft)]);
        // Following another link drops the way forward.
        app.follow(offset_of(&app, "[^2]"));
        assert!(app.forward.is_empty());
        drive(&ctx, &mut app, &mut time, vec![alt(egui::Key::ArrowRight)]);
        assert_eq!(app.selection().head, offset_of(&app, "Two."));
    }

    #[test]
    fn a_link_to_a_safe_document_opens_in_the_default_app() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "[chart](chart.pdf) [tool](tool.sh)\n").unwrap();
        fs::write(dir.path().join("chart.pdf"), "").unwrap();
        fs::write(dir.path().join("tool.sh"), "").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        app.follow(offset_of(&app, "chart"));
        assert_eq!(
            app.opened_urls,
            vec![dir.path().join("chart.pdf").display().to_string()]
        );
        app.follow(offset_of(&app, "tool"));
        assert_eq!(app.opened_urls.len(), 1, "a script isn't opened");
        assert!(app.hint.as_ref().unwrap().0.contains("tool.sh"));
        assert_eq!(app.doc.path(), Some(a.as_path()));
    }

    #[test]
    fn history_changes_only_when_a_navigation_lands() {
        // Review of #33.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "[next](b.md)\n").unwrap();
        fs::write(dir.path().join("b.md"), "# B\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        let gone = Place {
            path: Some(dir.path().join("gone.md")),
            offset: 0,
        };
        app.forward.push(gone.clone());
        // A cancelled open: no back entry, the way forward kept.
        type_into(&mut app, "draft");
        app.follow(1);
        assert!(app.confirm.is_some());
        app.confirm = None;
        drive(&ctx, &mut app, &mut time, vec![]);
        assert!(app.back.is_empty());
        assert_eq!(app.forward, vec![gone.clone()]);
        // Forward to a deleted file: refused, and the place stays.
        app.go_forward();
        assert_eq!(app.forward, vec![gone]);
        assert!(app.back.is_empty());
    }

    #[test]
    fn clicking_an_outline_heading_jumps_both_panes_and_goes_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Alpha\n\nbody\n\n## Beta\n\nend\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        let start = run.app.selection().head;
        let alpha = text(&run.app).find("Alpha").unwrap();
        let beta = text(&run.app).find("Beta").unwrap();
        let beta_row = run.text_rect("Beta").expect("Beta");
        assert!(
            beta_row.left() > 700.0,
            "the outline draws Beta, {beta_row:?}"
        );

        run.click_text("Beta");
        assert_eq!(run.app.code.selection(), Selection::caret(beta));
        assert_eq!(run.app.live.selection(), Selection::caret(beta));

        run.click_text("Alpha");
        assert_eq!(run.app.code.selection(), Selection::caret(alpha));
        assert_eq!(run.app.live.selection(), Selection::caret(alpha));

        run.key(egui::Key::ArrowLeft, egui::Modifiers::ALT);
        assert_eq!(run.app.code.selection(), Selection::caret(beta));
        assert_eq!(run.app.live.selection(), Selection::caret(beta));
        run.key(egui::Key::ArrowLeft, egui::Modifiers::ALT);
        assert_eq!(run.app.code.selection(), Selection::caret(start));
        assert_eq!(run.app.live.selection(), Selection::caret(start));

        run.click_text("Beta");
        run.frame(vec![egui::Event::Text("Q".into())]);
        assert!(text(&run.app).contains("QBeta"), "{}", text(&run.app));
        let edited = run.text_rect("QBeta").expect("QBeta");
        assert!(
            edited.left() > 700.0,
            "the outline shows the edited heading, {edited:?}"
        );
    }

    #[test]
    fn the_outline_stays_when_the_sidebar_is_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Alpha\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.app.sidebar.visible = false;
        run.frame(vec![]);
        let title = run.text_rect("Outline").expect("Outline");
        let row = run.text_rect("Alpha").expect("Alpha");
        assert!(title.left() > 700.0, "{title:?}");
        assert!(row.left() > 700.0, "{row:?}");
    }

    #[test]
    fn the_outline_toggle_is_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Alpha\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        assert!(run.text_rect("Outline").is_some());
        assert!(run.text_rect("«").is_some());
        assert!(run.text_rect("»").is_some());

        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::B, shift);
        assert!(!run.app.layout.outline_visible);
        assert!(run.text_rect("Outline").is_none());
        let stored = fs::read_to_string(dir.path().join("layout")).unwrap();
        assert!(stored.ends_with("0\n"), "{stored}");

        let ctx = egui::Context::default();
        let hidden = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert!(!hidden.layout.outline_visible);
        assert_eq!(hidden.layout.outline_width, outline::PREFERRED_WIDTH);

        run.click_text("»");
        assert!(run.app.layout.outline_visible);
        assert!(run.text_rect("Outline").is_some());
        run.click_text("«");
        assert!(!run.app.sidebar.visible);
        assert!(run.text_rect("All files").is_none());
        run.click_text("«");
        assert!(run.app.sidebar.visible);
        assert!(run.text_rect("All files").is_some());
    }

    #[test]
    fn f1_lists_the_keys_in_effect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "hello\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(run.app.show_keys);
        for _ in 0..3 {
            if run.text_rect("Key bindings").is_some() {
                break;
            }
            run.frame(vec![]);
        }
        assert!(run.text_rect("Key bindings").is_some());
        assert!(run.text_rect("Ctrl+E").is_some());
        assert!(run.text_rect("Cycle split, code, and live").is_some());
        assert!(run.text_rect("Ctrl+Shift+E").is_some());
        assert_eq!(
            run.app.keys.shortcut_text(Action::ToggleOutline),
            "Ctrl+Shift+B"
        );
        assert_eq!(
            run.app.keys.shortcut_text(Action::NewFolder),
            "Ctrl+Shift+N"
        );
        assert_eq!(run.app.keys.shortcut_text(Action::ShowKeys), "F1");

        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
        assert!(run.text_rect("Key bindings").is_none());

        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(run.app.show_keys);
        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
    }

    #[test]
    fn a_new_folder_is_created_beside_the_selection() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let chapter = notes.join("chapter");
        fs::create_dir_all(&chapter).unwrap();
        fs::write(notes.join("a.md"), "a\n").unwrap();
        let mut run = Run::new(dir.path(), Some(notes.join("a.md")));
        let start = Instant::now();
        while run.app.browser.row_rect(&chapter).is_none() {
            run.frame(vec![]);
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "chapter was not listed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        run.click_text("chapter");
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::N, shift);
        let prompt = run
            .app
            .new_folder
            .as_ref()
            .expect("Ctrl+Shift+N opens the prompt");
        assert_eq!(prompt.dir, chapter);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(
            run.app.new_folder.as_ref().unwrap().error.as_deref(),
            Some("Enter a folder name.")
        );
        assert!(!chapter.join("Pics").exists());

        run.frame(vec![egui::Event::Text("Pics".into())]);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        let created = chapter.join("Pics");
        assert!(created.is_dir());
        assert!(run.app.new_folder.is_none());
        let start = Instant::now();
        while run.app.browser.row_rect(&created).is_none() {
            run.frame(vec![]);
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "Pics was not listed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        run.click_text("chapter");
        run.click_text("⊞");
        assert_eq!(run.app.new_folder.as_ref().unwrap().dir, chapter);
        run.frame(vec![egui::Event::Text("Pics".into())]);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert!(
            run.app
                .new_folder
                .as_ref()
                .unwrap()
                .error
                .as_deref()
                .unwrap()
                .contains("already exists")
        );
        run.app.new_folder.as_mut().unwrap().name = "a/b".into();
        run.app.new_folder.as_mut().unwrap().error = None;
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(
            run.app.new_folder.as_ref().unwrap().error.as_deref(),
            Some("A folder name can't contain / or \\, or be . or ..")
        );
        assert!(!chapter.join("a").exists());
        run.app.new_folder.as_mut().unwrap().name = "..".into();
        run.app.new_folder.as_mut().unwrap().error = None;
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(
            run.app.new_folder.as_ref().unwrap().error.as_deref(),
            Some("A folder name can't contain / or \\, or be . or ..")
        );

        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(run.app.new_folder.is_none());

        run.click_text("∗");
        assert!(run.app.search.is_open());
        assert!(run.text_rect("Match case").is_some());
    }

    #[test]
    fn an_empty_file_says_it_has_no_headings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "just text\n").unwrap();
        let run = Run::new(dir.path(), Some(path));
        let title = run.text_rect("Outline").expect("Outline");
        let empty = run.text_rect("No headings").expect("No headings");
        assert!(title.left() > 700.0, "{title:?}");
        assert!(empty.left() > 700.0, "{empty:?}");
    }

    #[test]
    fn clicking_an_empty_heading_jumps_to_its_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "intro\n\n#\n\nbody\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        let found = inkmark_parse::headings(&run.app.doc, run.app.parse.output());
        assert_eq!(found[0].text, "");
        run.click_text("Empty heading");
        assert_eq!(run.app.code.selection(), Selection::caret(found[0].offset));
        assert_eq!(run.app.live.selection(), Selection::caret(found[0].offset));
    }

    #[test]
    fn outline_keeps_its_width_beside_the_sidebar() {
        assert_eq!(
            outline::column_widths(1000.0, Some(240.0), Some(200.0)),
            (240.0, 200.0)
        );
        assert_eq!(
            outline::column_widths(1000.0, None, Some(200.0)),
            (0.0, 200.0)
        );
        assert_eq!(
            outline::column_widths(400.0, Some(240.0), Some(200.0)),
            (160.0, 120.0)
        );
        assert_eq!(
            outline::column_widths(1000.0, Some(240.0), Some(360.0)),
            (240.0, 360.0)
        );
        // A wide preference still gives the panes their reserve on a narrow window.
        assert_eq!(
            outline::column_widths(400.0, Some(240.0), Some(360.0)),
            (160.0, 120.0)
        );
        assert_eq!(
            outline::column_widths(1000.0, Some(240.0), None),
            (240.0, 0.0)
        );
        assert_eq!(
            outline::column_widths(400.0, Some(240.0), None),
            (160.0, 0.0)
        );
    }

    #[test]
    fn go_to_line_moves_both_panes_and_clamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "one\ntwo\nthree").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        let two = text(&run.app).find("two").unwrap();
        let three = text(&run.app).find("three").unwrap();

        run.key(egui::Key::G, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        assert!(run.text_rect("Go to line").is_some());
        run.frame(vec![egui::Event::Text("2".into())]);
        run.frame(vec![]);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert_eq!(run.app.code.selection(), Selection::caret(two));
        assert_eq!(run.app.live.selection(), Selection::caret(two));
        assert!(run.text_rect("Go to line").is_none());

        run.key(egui::Key::ArrowLeft, egui::Modifiers::ALT);
        assert_eq!(run.app.code.selection(), Selection::caret(0));
        assert_eq!(run.app.live.selection(), Selection::caret(0));

        run.key(egui::Key::G, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("99".into())]);
        run.frame(vec![]);
        run.click_text("Go");
        run.frame(vec![]);
        assert_eq!(run.app.code.selection(), Selection::caret(three));
        assert_eq!(run.app.live.selection(), Selection::caret(three));
    }

    #[test]
    fn go_to_line_ignores_a_blank_entry_and_escape_closes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "one\ntwo\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.key(egui::Key::G, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("no".into())]);
        run.frame(vec![]);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert_eq!(run.app.code.selection().head, 0);
        assert!(run.text_rect("Go to line").is_some());
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert!(run.text_rect("Go to line").is_none());
        assert_eq!(run.app.code.selection().head, 0);
        assert_eq!(text(&run.app), "one\ntwo\n");
    }

    #[test]
    fn folding_a_heading_hides_its_body_in_the_code_pane() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        let src = "# Alpha\nvisible body\n## Beta\ninner body\n# Gamma\ntail\n";
        fs::write(&path, src).unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        assert!(run.app.code.measured_height(1) > 0.0);
        assert!(run.app.live.measured_height(1) > 0.0);

        run.click_code_mark("▾");
        let hidden = run.app.code.hidden_ranges().to_vec();
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        let body = text(&run.app).find("visible body").unwrap();
        let inner = text(&run.app).find("inner body").unwrap();
        let gamma = text(&run.app).find("# Gamma").unwrap();
        assert!(hidden[0].start <= body && inner < hidden[0].end);
        assert_eq!(hidden[0].end, gamma);
        assert_eq!(run.app.code.measured_height(1), 0.0);
        assert!(run.app.live.measured_height(1) > 0.0);
        assert_eq!(text(&run.app), src);

        run.app.code.request_focus(&run.ctx);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("\n".into())]);
        run.frame(vec![]);
        let shifted = run.app.code.hidden_ranges().to_vec();
        assert_eq!(shifted.len(), 1, "{shifted:?}");
        assert_eq!(shifted[0].start, hidden[0].start + 1);
        assert_eq!(shifted[0].end, hidden[0].end + 1);

        let inner = text(&run.app).find("inner body").unwrap();
        let line = run.app.doc.byte_to_line(inner) + 1;
        run.key(egui::Key::G, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text(line.to_string())]);
        run.frame(vec![]);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert_eq!(run.app.code.selection(), Selection::caret(inner));
        assert_eq!(run.app.live.selection(), Selection::caret(inner));
        assert!(
            run.app
                .code
                .hidden_ranges()
                .iter()
                .all(|range| inner < range.start || inner >= range.end),
            "{:?}",
            run.app.code.hidden_ranges()
        );
        let shown = run.app.doc.byte_to_line(inner);
        assert!(run.app.code.measured_height(shown) > 0.0);
        assert!(run.app.live.measured_height(shown) > 0.0);

        run.click_code_mark("▾");
        assert!(!run.app.code.hidden_ranges().is_empty());
        let other = dir.path().join("other.md");
        fs::write(&other, "plain\n").unwrap();
        run.app.open(other);
        run.frame(vec![]);
        assert!(run.app.code.hidden_ranges().is_empty());
    }

    #[test]
    fn enter_above_a_fold_keeps_it_and_shows_the_new_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Alpha\nbody line\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.click_code_mark("▾");
        run.app.code.request_focus(&run.ctx);
        run.frame(vec![]);
        run.key(egui::Key::End, egui::Modifiers::NONE);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert_eq!(text(&run.app), "# Alpha\n\nbody line\n");
        let hidden = run.app.code.hidden_ranges().to_vec();
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        assert!(
            run.app.code.measured_height(1) > 0.0,
            "the new line is drawn"
        );
        assert_eq!(run.app.code.measured_height(2), 0.0);
        let in_code = |rect: &egui::Rect| rect.left() > 220.0 && rect.left() < 520.0;
        assert!(
            run.text_rects("▸").iter().any(in_code),
            "the heading still shows the fold"
        );
        run.click_code_mark("▸");
        assert!(run.app.code.hidden_ranges().is_empty());
    }

    #[test]
    fn arrow_down_stops_on_the_last_visible_line_of_a_fold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Last\nhidden body").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.click_code_mark("▾");
        let hidden = run.app.code.hidden_ranges().to_vec();
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        run.app.code.request_focus(&run.ctx);
        run.frame(vec![]);
        run.key(egui::Key::ArrowDown, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert_eq!(run.app.code.selection().head, 0);
        assert_eq!(run.app.code.hidden_ranges(), hidden.as_slice());
        assert_eq!(run.app.code.measured_height(1), 0.0);
        assert_eq!(text(&run.app), "# Last\nhidden body");
    }

    #[test]
    fn a_jump_after_a_live_edit_rebases_folds_before_revealing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Alpha\nbody line\n# Gamma\ntail\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.click_code_mark("▾");
        let start = run.app.code.hidden_ranges()[0].start;
        let end = run.app.code.hidden_ranges()[0].end;
        // The code pane is not on screen, so it has not shifted the fold.
        run.app.mode = Mode::Live;
        let inserted = "xxxxxxxxxxxxxxxxxxxxxxxx";
        run.app
            .doc
            .apply(
                vec![inkmark_buffer::Edit::insert(0, inserted)],
                Selection::caret(0),
                Selection::caret(inserted.len()),
                inkmark_buffer::EditKind::Other,
            )
            .unwrap();
        // Inside the old fold range, outside the range once it shifts.
        run.app.code.set_selection(Selection::caret(start));
        run.app.mode = Mode::Code;
        run.frame(vec![]);
        let hidden = run.app.code.hidden_ranges().to_vec();
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        assert_eq!(hidden[0].start, start + inserted.len());
        assert_eq!(hidden[0].end, end + inserted.len());
        assert!(start < hidden[0].start);
    }

    #[test]
    fn structural_selection_runs_in_the_code_pane() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        let src = "one two\n\nsee [a](http://x.com) there\n";
        fs::write(&path, src).unwrap();
        let mut run = Run::new(dir.path(), Some(path));

        run.key(egui::Key::D, egui::Modifiers::COMMAND);
        let word = run.app.code.selection().range();
        assert_eq!(&text(&run.app)[word.clone()], "one");
        assert_eq!(run.app.live.selection().range(), word);

        run.key(
            egui::Key::P,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        let para = run.app.code.selection().range();
        assert_eq!(&text(&run.app)[para.clone()], "one two\n");
        assert_eq!(run.app.live.selection().range(), para);

        let open = text(&run.app).find('[').unwrap();
        let close = text(&run.app).find(']').unwrap();
        run.app.code.set_selection(Selection::caret(open));
        run.frame(vec![]);
        run.key(
            egui::Key::Backslash,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert_eq!(run.app.code.selection(), Selection::caret(close));
        assert_eq!(run.app.live.selection(), Selection::caret(close));
        run.key(
            egui::Key::Backslash,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert_eq!(run.app.code.selection(), Selection::caret(open));

        // The live pane swallows the chord and leaves the caret where it is.
        run.app.focus_pane(&run.ctx, Pane::Live);
        run.frame(vec![]);
        run.key(egui::Key::D, egui::Modifiers::COMMAND);
        run.key(
            egui::Key::Backslash,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert_eq!(text(&run.app), src);
        assert_eq!(run.app.live.selection(), Selection::caret(open));
        assert_eq!(run.app.code.selection(), Selection::caret(open));
    }

    #[test]
    fn structural_selection_inside_a_fold_opens_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        let src = "# Title\n\n```\ncode\n```\n\n# Next\n";
        fs::write(&path, src).unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.click_code_mark("▾");
        let open = text(&run.app).find("```").unwrap();
        let close = text(&run.app).rfind("```").unwrap();
        assert!(
            run.app
                .code
                .hidden_ranges()
                .iter()
                .any(|range| range.start <= open && close < range.end),
            "{:?}",
            run.app.code.hidden_ranges()
        );

        run.app.code.request_focus(&run.ctx);
        run.app.code.mirror_selection(Selection::caret(open));
        run.key(
            egui::Key::Backslash,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        run.frame(vec![]);
        assert_eq!(run.app.code.selection(), Selection::caret(close));
        assert_eq!(run.app.live.selection(), Selection::caret(close));
        assert!(
            run.app
                .code
                .hidden_ranges()
                .iter()
                .all(|range| close < range.start || close >= range.end),
            "{:?}",
            run.app.code.hidden_ranges()
        );
        let line = run.app.doc.byte_to_line(close);
        assert!(run.app.code.measured_height(line) > 0.0);
        assert_eq!(text(&run.app), src);
    }
}
