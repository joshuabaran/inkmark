use std::collections::{HashSet, VecDeque};
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
use inkmark_parse::{GfmParser, MarkdownParser, ParseState};
use inkmark_text::Fonts;
use inkmark_view::keys::{self, Action, Scope};
use inkmark_view::{BrowserOutput, Cheatsheet, CodeView, FileBrowser, LiveView, ScrollPos, theme};

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
/// Ctrl+Shift+T remembers this many closed tabs. Older ones are dropped.
const CLOSED_TABS: usize = 10;
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
mod session_tabs;
mod tabs;

use findbar::FindState;
use folder_search::{FolderSearch, SearchChoice, SearchOpen};
use gotoline::GoToLine;
pub(crate) use layout::Mode;
mod sidebar;

const USAGE: &str = "\
Usage: inkmark [FILE | FOLDER]

  FILE     open it beside the restored tabs, browsing its folder
           (a missing path becomes a new file)
  FOLDER   browse it, with nothing open
           (no argument: restore the last session's tabs,
            browsing the current directory)

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
#[derive(Clone, Copy, PartialEq, Eq)]
enum Banner {
    /// Another program changed the file since we loaded or saved it.
    DiskChanged,
    DiskMissing,
}

/// A save or discard the user has not answered yet.
///
/// `Close` is the window, and one tab is unsaved: the id is that tab, so
/// the prompt still names it if another tab is showing when the answer
/// comes back. `CloseAll` is the window, and more than one tab is unsaved.
/// `CloseTab` is that tab only. The strip index can move before the answer
/// comes back, so both single-tab questions store the stable id.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Confirm {
    Close(u64),
    CloseAll,
    CloseTab(u64),
}

/// What a successful save should do afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterSave {
    /// Close the window once every tab is saved.
    Window,
    /// Close this tab once its save lands.
    Tab(u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DialogKind {
    Open,
    /// Save As for this tab. The picker outlives the tab that was on screen
    /// when it opened.
    SaveAs(u64),
    Folder,
    /// Pick the folder to move this file or folder into.
    MoveTo(PathBuf),
}

enum DialogResult {
    Open(Option<PathBuf>),
    /// The tab that opened Save As, and the path it chose.
    SaveAs(u64, Option<PathBuf>),
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

/// A caret placement waiting on the tab an open landed in. Applied once
/// that tab's parse has settled, even if another tab is showing. Dropped
/// if that tab is gone or its file is no longer `path`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingJump {
    tab: u64,
    path: PathBuf,
    jump: Jump,
}

/// A history change waiting on the tab an open landed in. Committed once
/// that tab is open on `path`, undone if the open failed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingHistory {
    tab: u64,
    path: PathBuf,
    here: Place,
    travel: Travel,
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

/// One open document: its text, its undo stack, and the view state that
/// belongs to it. The window holds a list of these.
///
/// Each tab's panes take egui ids from `id`, so scroll and focus stay
/// with that tab.
///
/// Keys, font size, line height, and the minimap flags are window
/// settings. `install_keys`, `apply_settings`, and the minimap toggle
/// write them onto every tab that already exists. A tab added later is
/// built from the active tab, which those writers have already updated.
struct Tab {
    /// Stable for the life of the tab. The first tab is 1.
    id: u64,
    doc: Document,
    parse: ParseState,
    code: CodeView,
    live: LiveView,
    /// The pane with keyboard focus (or that last had it).
    focus: Pane,
    banner: Option<Banner>,
    /// An external change or deletion not yet answered. Shown on the tab
    /// and the sidebar immediately. The banner waits until the tab is showing.
    disk: Option<Banner>,
    /// Query and matches for this document. The bar's open flag is on `App`.
    find: FindState,
    /// Replaced by the next preview open. A double click or an edit clears
    /// it. The strip draws the title in italic while it is set.
    preview: bool,
}

impl Tab {
    fn title(&self) -> String {
        self.doc
            .path()
            .and_then(|path| path.file_name())
            .map_or_else(
                || "untitled".into(),
                |name| name.to_string_lossy().into_owned(),
            )
    }

    fn selection(&self) -> inkmark_buffer::Selection {
        match self.focus {
            Pane::Code => self.code.selection(),
            Pane::Live => self.live.selection(),
        }
    }
}

/// What `open_existing` did with a path that was supposed to be a file.
enum ExistingOpen {
    Opened,
    /// Not on disk. The caller drops it. A new file is not created.
    Missing,
    /// On disk, but it could not be read. `String` is the banner text.
    Failed(String),
}

/// A tab Ctrl+Shift+T can open again. The text is not kept: the file is
/// read from disk, at this caret and scroll. It comes back pinned.
struct ClosedTab {
    path: PathBuf,
    caret: usize,
    scroll: ScrollPos,
}

struct App {
    tabs: Vec<Tab>,
    active: usize,
    /// Front is the active tab once Ctrl is released. While Ctrl is held,
    /// [`Self::mru_at`] selects an entry and this order stays put. A click,
    /// an open, or Alt+1 moves that tab to the front instead.
    mru: Vec<usize>,
    /// Index into [`Self::mru`] while Ctrl+Tab is held. `None` once the
    /// walk has settled.
    mru_at: Option<usize>,
    /// The next tab's egui-id salt. The first tab is 1.
    next_tab_id: u64,
    /// Epoch of the active document after the last open or tab switch.
    /// An edit moves it, which pins a preview tab.
    doc_epoch: u64,
    /// The click that opened a tab focuses the sidebar for that pass.
    /// The next frame puts focus on the opened tab's pane.
    refocus_pane: bool,
    /// Kept so a tab opened later can wake its parse worker and take focus.
    ctx: egui::Context,
    /// One image decoder for every live view. A picture used by two
    /// documents is decoded once.
    images: Option<inkmark_view::ImageCache>,
    mode: Mode,
    /// The last thing that failed (open, save, a dialog), until dismissed.
    error: Option<String>,
    /// A pane's explanation for a key that did nothing, shown for a moment.
    hint: Option<(String, Instant)>,
    /// Where followed links were clicked, most recent last (Alt+Left).
    back: Vec<Place>,
    /// Places gone back from, most recent last (Alt+Right).
    forward: Vec<Place>,
    /// A history change waiting on the tab an open landed in.
    pending_history: Option<PendingHistory>,
    /// A caret placement waiting on the tab an open landed in.
    pending_jump: Option<PendingJump>,
    /// Tests record URLs here instead of starting a browser.
    #[cfg(test)]
    opened_urls: Vec<String>,
    dialog: Option<Receiver<DialogResult>>,
    confirm: Option<Confirm>,
    after_save: Option<AfterSave>,
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
    /// The recent-files dialog is open, with this entry selected. The empty
    /// window draws its list in the pane and leaves this `None`.
    recent_list: Option<usize>,
    /// Which recent file the empty pane has selected.
    empty_recent: usize,
    browser: FileBrowser,
    sidebar: sidebar::Sidebar,
    /// Mode, minimaps, the split, and the outline width, restored on open.
    layout: layout::Layout,
    /// `$XDG_STATE_HOME/inkmark/tabs`, beside the layout file.
    session_store: Option<PathBuf>,
    /// The session file as last written, or as it was when we left it alone.
    session_saved: String,
    /// This window's tabs are the session. A folder launch leaves this
    /// false so browsing does not wipe the tabs saved by the last window.
    session_adopted: bool,
    /// A restore is opening tabs. Writes wait until that list is finished.
    session_quiet: bool,
    /// Still on disk, but not opened: unreadable, or not UTF-8. Missing
    /// paths are not kept. These stay in the session file so the next
    /// launch can try again.
    session_unopened: Vec<session_tabs::StoredTab>,
    /// Closed tabs, oldest first. Ctrl+Shift+T takes from the back.
    closed: VecDeque<ClosedTab>,
    /// Drawn outline width where the current drag began.
    outline_drag_from: Option<f32>,
    /// Drawn split position where the current drag began.
    split_drag_from: Option<f32>,
    /// Asks for a name, then creates the file and opens it.
    new_file: Option<NewFilePrompt>,
    /// Asks for a name, then creates the folder and selects it.
    new_folder: Option<NewFilePrompt>,
    /// Ctrl+/ and F1. The chords in effect, grouped, filtered by `keys_query`.
    show_keys: bool,
    /// The overlay's filter. Cleared each time the overlay opens.
    keys_query: String,
    /// The key list or the syntax cheatsheet had the keyboard last frame.
    /// The frame one of them closes still has the modal layer, so the pane
    /// surrenders focus while it draws and has to be focused again afterwards.
    keys_return_focus: bool,
    /// Ask the filter for focus once, on the frame the overlay opens.
    /// Asking again each frame interrupts IME composition.
    keys_focus_filter: bool,
    /// The pane was given focus last frame, before its arrow lock could
    /// stick. This frame the pane's own lock applies, and an arrow must
    /// not move egui's focus away from it.
    keys_arm_lock: bool,
    /// Ctrl+Shift+/. The Markdown syntax cheatsheet.
    show_syntax: bool,
    /// Rows for the cheatsheet. Drawn only while `show_syntax` is set.
    syntax: Cheatsheet,
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
    /// The find bar is showing. The query itself is on the active tab, so
    /// F3 still repeats it after the bar closes.
    find_open: bool,
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

/// Caret offsets that still land on a character of `doc`.
fn clamp_selection(selection: Selection, doc: &Document) -> Selection {
    Selection {
        anchor: clamp_offset(doc, selection.anchor),
        head: clamp_offset(doc, selection.head),
    }
}

fn clamp_offset(doc: &Document, offset: usize) -> usize {
    let at = offset.min(doc.len());
    if doc.is_char_boundary(at) {
        at
    } else {
        doc.prev_char_boundary(at)
    }
}

/// The caret and the scroll of the pane that has the keyboard.
fn tab_place(tab: &Tab) -> (usize, ScrollPos) {
    let caret = tab.selection().head;
    let scroll = match tab.focus {
        Pane::Code => tab.code.scroll_pos(),
        Pane::Live => tab.live.scroll_pos(&tab.doc, tab.parse.output()),
    };
    (caret, scroll)
}

fn clamp_scroll(pos: ScrollPos, lines: usize) -> ScrollPos {
    if lines == 0 || pos.line >= lines {
        ScrollPos {
            line: lines.saturating_sub(1),
            frac: 0.0,
        }
    } else {
        ScrollPos {
            line: pos.line,
            frac: pos.frac.clamp(0.0, 1.0),
        }
    }
}

impl App {
    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

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
        let session_store = state_dir.as_ref().map(|dir| dir.join("tabs"));
        let session_saved = session_store
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        let layout = layout::Layout::load(layout_store);
        let mode = layout.mode;
        let code_minimap = layout.code_minimap;
        let live_minimap = layout.live_minimap;
        let search_ctx = ctx.clone();
        let parse_ctx = ctx.clone();
        let mut app = Self {
            tabs: vec![Tab {
                id: 1,
                doc: Document::default(),
                code: CodeView::with_fonts(fonts.clone(), egui::Id::new(("code_view", 1u64))),
                live: LiveView::with_fonts(fonts.clone(), egui::Id::new(("live_view", 1u64))),
                // GitHub Flavored Markdown; PulldownParser is plain CommonMark.
                parse: ParseState::new(Arc::new(GfmParser), &Document::default(), move || {
                    parse_ctx.request_repaint()
                }),
                // Live-only must be the focus before the first restore.
                // Focusing the code pane from here switches the mode to
                // code and writes that into the layout file.
                focus: if mode == Mode::Live {
                    Pane::Live
                } else {
                    Pane::Code
                },
                banner: None,
                disk: None,
                find: FindState::default(),
                preview: false,
            }],
            active: 0,
            mru: vec![0],
            mru_at: None,
            next_tab_id: 2,
            doc_epoch: 0,
            refocus_pane: false,
            ctx: ctx.clone(),
            images: None,
            mode,
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
            after_save: None,
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
            empty_recent: 0,
            browser: FileBrowser::new(root.clone()),
            sidebar: sidebar::Sidebar::load(sidebar_store),
            layout,
            session_store,
            session_saved,
            session_adopted: false,
            session_quiet: false,
            session_unopened: Vec::new(),
            closed: VecDeque::new(),
            outline_drag_from: None,
            split_drag_from: None,
            new_file: None,
            new_folder: None,
            show_keys: false,
            keys_query: String::new(),
            keys_return_focus: false,
            keys_focus_filter: false,
            keys_arm_lock: false,
            show_syntax: false,
            syntax: Cheatsheet::new(fonts.clone()),
            rename: None,
            trash_confirm: None,
            trash: Box::new(SystemTrash),
            os_theme: os_theme::OsTheme::fixed(),
            fonts: fonts.clone(),
            font_files: Vec::new(),
            font_stamps: Vec::new(),
            settings: config::Settings::default(),
            keys: keys::KeyMap::builtin(),
            find_open: false,
            goto: GoToLine::default(),
            search: FolderSearch::new(root, move || search_ctx.request_repaint()),
            outline: outline::Outline::default(),
            settings_error: None,
            modal_was_open: false,
            #[cfg(test)]
            dialog_hook: None,
        };
        // No argument reopens the saved tabs, then offers recent files when
        // that list was empty. A file opens beside those tabs. A folder
        // browses that folder with nothing open and leaves the saved tabs
        // for the next launch that asks for them.
        let restore = match &launch {
            Launch::File { .. } => true,
            Launch::Folder { .. } => path.is_none(),
        };
        if restore {
            app.restore_session();
        }
        // Opening the file argument replaces the blank tab and clears the
        // banner. A session file that could not be read still needs to be
        // reported, or the launch looks like it succeeded.
        let session_error = app.error.clone();
        match launch {
            Launch::File { file, .. } => app.open(file),
            Launch::Folder { .. } => {
                if path.is_none() && app.sole_blank_tab() && !app.recent.entries().is_empty() {
                    app.recent_list = Some(0);
                }
            }
        }
        if let Some(earlier) = session_error {
            app.error = Some(match app.error.as_deref() {
                Some(later) if later != earlier => format!("{earlier}. {later}"),
                _ => earlier,
            });
        }
        for tab in &mut app.tabs {
            tab.code.show_minimap = code_minimap;
            tab.live.show_minimap = live_minimap;
        }
        app.install_keys();
        // A live-only window keeps the keyboard on the pane that is showing.
        if app.mode == Mode::Live {
            app.tab_mut().focus = Pane::Live;
            app.tab_mut().live.request_focus(ctx);
        } else {
            app.tab_mut().code.request_focus(ctx);
        }
        app
    }

    /// Copies the current shortcuts onto the sidebar and every tab's panes.
    fn install_keys(&mut self) {
        let keys = self.keys.clone();
        for tab in &mut self.tabs {
            tab.code.set_keys(keys.clone());
            tab.live.set_keys(keys.clone());
        }
        self.browser.set_keys(keys);
    }

    /// Focuses the tab that already has `path`, or opens it in a new pinned
    /// tab. The one blank untitled tab a window starts with is replaced, so
    /// launching a file does not leave an empty tab beside it.
    fn request_open(&mut self, path: PathBuf) {
        self.open_tab(path, true);
    }

    fn open(&mut self, path: PathBuf) {
        self.open_tab(path, true);
    }

    /// `pin` keeps the tab. Otherwise the file goes in the one preview tab
    /// (italic title), which the next preview open replaces. A file that is
    /// already open is focused, and a pin request pins that tab.
    fn open_tab(&mut self, path: PathBuf, pin: bool) {
        self.pin_edited_preview();
        let was_quiet = self.session_quiet;
        self.session_quiet = true;
        let changed = self.open_tab_now(&path, pin);
        self.session_quiet = was_quiet;
        if changed {
            self.touch_session();
        }
    }

    /// Opens `path`. `true` when a tab was added, replaced, focused, or pinned.
    fn open_tab_now(&mut self, path: &Path, pin: bool) -> bool {
        if let Some(index) = self.tab_index(path) {
            self.recent.add(path);
            if pin {
                self.tabs[index].preview = false;
            }
            self.activate(index);
            return true;
        }
        let doc = match Document::open(path) {
            Ok(doc) => doc,
            // A path that doesn't exist yet becomes a new file there.
            Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut doc = Document::default();
                doc.set_path(path);
                doc
            }
            Err(e) => {
                self.error = Some(format!("Couldn't open {}: {e}", path.display()));
                return false;
            }
        };
        // Only files that opened (or are deliberately new) are remembered.
        self.recent.add(path);
        if !pin && let Some(index) = self.tabs.iter().position(|tab| tab.preview) {
            // An edit pins a preview, and so does an unanswered disk alert.
            // Replacing the slot would drop the only buffer of a missing file.
            if self.tabs[index].doc.is_dirty() || self.tabs[index].disk.is_some() {
                self.tabs[index].preview = false;
            } else {
                self.activate(index);
                self.replace_document(doc);
                self.tabs[self.active].preview = true;
                self.focus_active();
                return true;
            }
        }
        if self.sole_blank_tab() {
            self.replace_document(doc);
            self.tabs[self.active].preview = !pin;
            self.note_mru(self.active);
            self.focus_active();
        } else {
            self.push_tab(doc);
            if !pin {
                self.tabs[self.active].preview = true;
            }
        }
        true
    }

    /// The window's starting document: nothing saved, nothing typed.
    fn sole_blank_tab(&self) -> bool {
        match self.tabs.as_slice() {
            [tab] => tab.doc.path().is_none() && !tab.doc.is_dirty() && tab.doc.is_empty(),
            _ => false,
        }
    }

    /// The tab whose file is `path`, counting two spellings of one file
    /// (`notes/a.md` and `notes/sub/../a.md`) as the same tab.
    fn tab_index(&self, path: &Path) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.doc.path().is_some_and(|open| same_file(open, path)))
    }

    fn activate(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let switched = index != self.active;
        if switched {
            self.pin_edited_preview();
        }
        self.active = index;
        self.note_mru(index);
        self.reveal_disk();
        self.focus_active();
        if switched {
            self.remember_epoch();
            self.remember_session();
        }
    }

    /// The active document's epoch, so a later edit can pin a preview.
    fn remember_epoch(&mut self) {
        self.doc_epoch = self
            .tabs
            .get(self.active)
            .map(|tab| tab.doc.epoch())
            .unwrap_or(0);
    }

    /// An edit since the last open or tab switch pins the preview tab.
    fn pin_edited_preview(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.preview && tab.doc.epoch() != self.doc_epoch {
            self.tabs[self.active].preview = false;
            self.remember_session();
        }
    }

    /// A middle click asked to pin, and the destination is this document.
    fn pin_if(&mut self, pin: bool) {
        if pin && self.tabs.get(self.active).is_some_and(|tab| tab.preview) {
            self.tab_mut().preview = false;
            self.remember_session();
        }
    }

    /// The on-screen tab shows a disk banner it picked up in the background.
    fn reveal_disk(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        if let Some(mark) = self.tab().disk {
            self.tab_mut().banner = Some(mark);
        }
    }

    /// Moves `index` to the front of the MRU list and ends a Ctrl+Tab walk.
    fn note_mru(&mut self, index: usize) {
        self.mru_at = None;
        self.mru.retain(|slot| *slot != index);
        self.mru.insert(0, index);
    }

    /// Ctrl+Tab walks most-recently-used order. The list stays put while
    /// Ctrl is held, so a second press walks further. `note_mru` here would
    /// undo that and only ever toggle the last two tabs.
    fn cycle_mru(&mut self, forward: bool) {
        if self.mru.len() < 2 {
            return;
        }
        let n = self.mru.len();
        let cursor = self.mru_at.unwrap_or(0);
        let next = if forward {
            (cursor + 1) % n
        } else {
            (cursor + n - 1) % n
        };
        let index = self.mru[next];
        let leaving = self.active;
        if index != leaving {
            // The tab we leave keeps a pin if this frame edited it.
            // Remembering the landed tab stops the frame-end check from
            // pinning a preview that was only switched to.
            self.pin_edited_preview();
        }
        self.mru_at = Some(next);
        self.active = index;
        self.reveal_disk();
        self.focus_active();
        if index != leaving {
            self.remember_epoch();
        }
    }

    /// Ctrl went up: the tab on screen becomes the most recent.
    fn end_mru_walk(&mut self) {
        let Some(at) = self.mru_at.take() else {
            return;
        };
        let Some(&index) = self.mru.get(at) else {
            return;
        };
        self.note_mru(index);
        self.remember_session();
    }

    /// Ctrl+PgUp / Ctrl+PgDn. `delta` is in strip order, and it wraps.
    fn move_tab(&mut self, delta: isize) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        let next = (self.active as isize + delta).rem_euclid(n as isize) as usize;
        self.activate(next);
    }

    /// Alt+1 through Alt+9. A tab that is not open is left alone.
    fn jump_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.activate(index);
        }
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if self.tabs[index].doc.is_dirty() {
            if index != self.active {
                self.activate(index);
            }
            let id = self.tabs[index].id;
            self.confirm = Some(Confirm::CloseTab(id));
            return;
        }
        self.remove_tab(index);
    }

    fn index_of_tab(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    /// Drops the tab. The last one leaves the window open. Recent files are
    /// drawn in the empty pane, not as a dialog, so the sidebar still gets clicks.
    fn remove_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let had_path = self.tabs[index].doc.path().is_some();
        self.note_closed(index);
        let shown = self.tabs.get(self.active).map(|tab| tab.id);
        let id = self.tabs[index].id;
        self.forget_tab_images(id);
        self.tabs.remove(index);
        self.mru.retain(|slot| *slot != index);
        for slot in &mut self.mru {
            if *slot > index {
                *slot -= 1;
            }
        }
        self.mru_at = None;
        if self.tabs.is_empty() {
            self.active = 0;
            self.mru.clear();
            self.doc_epoch = 0;
            self.confirm = None;
            self.after_save = None;
            self.find_open = false;
            self.goto.close();
            self.recent_list = None;
            self.empty_recent = 0;
        } else {
            self.active = self.mru[0];
            // The closed tab is already gone. Remember the landed document
            // when it is a different tab, and do not pin it: that would compare
            // its epoch with the tab we just left. Closing a background tab
            // keeps the same document, so an edit in this frame can still pin it.
            if self.tabs.get(self.active).map(|tab| tab.id) != shown {
                self.remember_epoch();
            }
            self.reveal_disk();
            self.focus_active();
        }
        // A blank tab was never in the session. Closing it must not adopt
        // the file and erase the tabs a folder launch left untouched.
        if had_path {
            self.touch_session();
        } else {
            self.remember_session();
        }
    }

    /// Remembers a closed file so Ctrl+Shift+T can open it again. A tab
    /// with no path has nothing on disk to reopen.
    fn note_closed(&mut self, index: usize) {
        let Some(path) = self.tabs[index].doc.path().map(Path::to_path_buf) else {
            return;
        };
        let (caret, scroll) = tab_place(&self.tabs[index]);
        self.closed.push_back(ClosedTab {
            path,
            caret,
            scroll,
        });
        while self.closed.len() > CLOSED_TABS {
            self.closed.pop_front();
        }
    }

    /// Opens the most recently closed file that is still on disk. A missing
    /// file is skipped. The reopened tab is pinned, so it does not replace
    /// the preview that is open now.
    fn reopen_closed(&mut self) {
        while let Some(closed) = self.closed.pop_back() {
            if !closed.path.is_file() {
                continue;
            }
            let was_quiet = self.session_quiet;
            self.session_quiet = true;
            let opened = self.reopen_one(&closed);
            self.session_quiet = was_quiet;
            if opened {
                self.touch_session();
                return;
            }
            self.closed.push_back(closed);
            return;
        }
    }

    fn reopen_one(&mut self, closed: &ClosedTab) -> bool {
        if self.tab_index(&closed.path).is_none()
            && !matches!(self.open_existing(&closed.path, true), ExistingOpen::Opened)
        {
            return false;
        }
        let Some(index) = self.tab_index(&closed.path) else {
            return false;
        };
        self.place_in_tab(index, closed.caret, closed.scroll);
        self.tabs[index].preview = false;
        self.activate(index);
        true
    }

    /// Opens a file that is already on disk. A missing path is not created.
    /// `remember_recent` is false while restoring, so a launch does not
    /// reshuffle the recent-files list. A file that is there but cannot be
    /// read is `ExistingOpen::Failed`, and the caller decides whether to
    /// keep it in the session.
    fn open_existing(&mut self, path: &Path, remember_recent: bool) -> ExistingOpen {
        if let Some(index) = self.tab_index(path) {
            self.forget_unopened(path);
            self.activate(index);
            return ExistingOpen::Opened;
        }
        let doc = match Document::open(path) {
            Ok(doc) => doc,
            Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return ExistingOpen::Missing;
            }
            Err(e) => {
                let message = format!("Couldn't open {}: {e}", path.display());
                self.error = Some(message.clone());
                return ExistingOpen::Failed(message);
            }
        };
        self.forget_unopened(path);
        if remember_recent {
            self.recent.add(path);
        }
        if self.sole_blank_tab() {
            self.replace_document(doc);
            self.tabs[self.active].preview = false;
            self.note_mru(self.active);
            self.focus_active();
        } else {
            self.push_tab(doc);
        }
        ExistingOpen::Opened
    }

    /// `path` opened, so a failed restore of it must not be written again.
    fn forget_unopened(&mut self, path: &Path) {
        self.session_unopened
            .retain(|kept| !same_file(&kept.path, path));
    }

    /// Puts the caret and the scroll back without scrolling to the caret.
    /// A reveal would drop the scroll position.
    fn place_in_tab(&mut self, index: usize, caret: usize, scroll: ScrollPos) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        let caret = clamp_offset(&tab.doc, caret);
        let scroll = clamp_scroll(scroll, tab.doc.line_count());
        let selection = Selection::caret(caret);
        tab.code.mirror_selection(selection);
        tab.code.set_scroll_pos(scroll);
        tab.live.mirror_selection(selection);
        let Tab {
            doc, parse, live, ..
        } = tab;
        live.set_scroll_pos(doc, parse.output(), scroll);
    }

    /// No file argument, or a file argument: put the saved tabs back.
    /// A missing file is skipped. A file that is there but cannot be read
    /// stays in the list, and the error is shown. A store that is not a
    /// session is left as it is.
    fn restore_session(&mut self) {
        let Some(path) = self.session_store.clone() else {
            return;
        };
        let Some(session) = session_tabs::load(&path) else {
            return;
        };
        self.session_quiet = true;
        let mut slots = Vec::with_capacity(session.tabs.len());
        let mut failed = Vec::new();
        for stored in &session.tabs {
            if !stored.path.is_file() {
                slots.push(None);
                continue;
            }
            if self.tab_index(&stored.path).is_none() {
                match self.open_existing(&stored.path, false) {
                    ExistingOpen::Opened => {}
                    ExistingOpen::Missing => {
                        slots.push(None);
                        continue;
                    }
                    ExistingOpen::Failed(message) => {
                        // A later tab's open clears `error`. Hold the message
                        // and the path: a missing file is the only one dropped.
                        failed.push(message);
                        self.session_unopened.push(stored.clone());
                        slots.push(None);
                        continue;
                    }
                }
            }
            let Some(index) = self.tab_index(&stored.path) else {
                slots.push(None);
                continue;
            };
            self.place_in_tab(
                index,
                stored.caret,
                ScrollPos {
                    line: stored.scroll_line,
                    frac: stored.scroll_frac,
                },
            );
            slots.push(Some(index));
        }
        let chosen = slots
            .get(session.active)
            .copied()
            .flatten()
            .or_else(|| {
                let below = session.active.min(slots.len());
                slots[..below].iter().rev().find_map(|slot| *slot)
            })
            .or_else(|| slots.iter().find_map(|slot| *slot));
        if let Some(index) = chosen {
            self.activate(index);
        }
        for tab in &mut self.tabs {
            tab.preview = false;
        }
        let preview_at = slots
            .iter()
            .enumerate()
            .find_map(|(i, slot)| {
                let index = (*slot)?;
                (session.tabs.get(i)?.preview && chosen == Some(index)).then_some(index)
            })
            .or_else(|| {
                slots.iter().enumerate().rev().find_map(|(i, slot)| {
                    let index = (*slot)?;
                    session.tabs.get(i)?.preview.then_some(index)
                })
            });
        if let Some(index) = preview_at
            && let Some(tab) = self.tabs.get_mut(index)
        {
            tab.preview = true;
        }
        if !failed.is_empty() {
            self.error = Some(failed.join(". "));
        }
        self.session_quiet = false;
        self.session_adopted = true;
        self.remember_session();
    }

    /// The open files, in strip order, as the session file would store them.
    fn session_snapshot(&self) -> session_tabs::Session {
        let mut tabs = Vec::new();
        let mut active = 0;
        for (index, tab) in self.tabs.iter().enumerate() {
            let Some(path) = tab.doc.path() else {
                continue;
            };
            let path = session_tabs::canonical(path);
            if path.to_str().is_none() {
                continue;
            }
            if index == self.active {
                active = tabs.len();
            }
            let (caret, scroll) = tab_place(tab);
            tabs.push(session_tabs::StoredTab {
                path,
                preview: tab.preview,
                caret,
                scroll_line: scroll.line,
                scroll_frac: scroll.frac,
            });
        }
        for kept in &self.session_unopened {
            let path = session_tabs::canonical(&kept.path);
            if path.to_str().is_none() || !path.is_file() {
                continue;
            }
            if tabs.iter().any(|tab| same_file(&tab.path, &path)) {
                continue;
            }
            tabs.push(session_tabs::StoredTab {
                path,
                preview: kept.preview,
                caret: kept.caret,
                scroll_line: kept.scroll_line,
                scroll_frac: kept.scroll_frac,
            });
        }
        session_tabs::Session { active, tabs }
    }

    /// Writes the session when this window owns it and the text changed.
    fn remember_session(&mut self) {
        if self.session_quiet || !self.session_adopted {
            return;
        }
        let Some(store) = self.session_store.clone() else {
            return;
        };
        let session = self.session_snapshot();
        let text = session_tabs::encode(&session);
        if text == self.session_saved {
            return;
        }
        if session_tabs::save(&store, &session).is_ok() {
            self.session_saved = text;
        }
    }

    /// This window's tabs are now the session.
    fn touch_session(&mut self) {
        if self.session_quiet {
            return;
        }
        self.session_adopted = true;
        self.remember_session();
    }

    /// A dialog that just closed must not leave the recent-files dialog up.
    /// With no document, the pane already shows that list.
    fn show_recent_if_empty(&mut self) {
        if self.tabs.is_empty() {
            self.recent_list = None;
        }
    }

    fn focus_active(&mut self) {
        if self.tabs.is_empty() {
            self.refocus_pane = false;
            return;
        }
        // A sidebar click focuses the tree in the same pass, after this
        // request, so the next frame asks again once that click is over.
        self.refocus_pane = true;
        let pane = self.tab().focus;
        let ctx = self.ctx.clone();
        self.focus_pane(&ctx, pane);
    }

    /// A pinned tab after the ones already open, using the window's current
    /// keys, font size, line height, and minimap flags.
    fn push_tab(&mut self, doc: Document) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let was_empty = self.tabs.is_empty();
        let (code_size, code_height, code_minimap, text_size, text_height, live_minimap) =
            if let Some(source) = self.tabs.get(self.active) {
                (
                    source.code.font_size,
                    source.code.line_height,
                    source.code.show_minimap,
                    source.live.font_size,
                    source.live.line_height,
                    source.live.show_minimap,
                )
            } else {
                let code_size = self.settings.code_size.unwrap_or(CODE_SIZE);
                let text_size = self.settings.text_size.unwrap_or(TEXT_SIZE);
                (
                    code_size,
                    (code_size * CODE_LINE).round(),
                    self.layout.code_minimap,
                    text_size,
                    (text_size * TEXT_LINE).round(),
                    self.layout.live_minimap,
                )
            };
        let fonts = self.fonts.clone();
        let keys = self.keys.clone();
        let ctx = self.ctx.clone();
        let focus = if self.mode == Mode::Live {
            Pane::Live
        } else {
            Pane::Code
        };
        let mut code = CodeView::with_fonts(fonts.clone(), egui::Id::new(("code_view", id)));
        code.font_size = code_size;
        code.line_height = code_height;
        code.show_minimap = code_minimap;
        code.set_keys(keys.clone());
        let mut live = LiveView::with_fonts(fonts, egui::Id::new(("live_view", id)));
        live.font_size = text_size;
        live.line_height = text_height;
        live.show_minimap = live_minimap;
        live.set_keys(keys);
        let parse = ParseState::new(Arc::new(GfmParser), &doc, move || ctx.request_repaint());
        self.tabs.push(Tab {
            id,
            doc,
            parse,
            code,
            live,
            focus,
            banner: None,
            disk: None,
            find: FindState::default(),
            preview: false,
        });
        self.active = self.tabs.len() - 1;
        self.note_mru(self.active);
        self.error = None;
        if was_empty {
            self.recent_list = None;
        }
        self.remember_epoch();
        self.focus_active();
    }

    fn replace_document(&mut self, doc: Document) {
        let id = self.tab().id;
        self.forget_tab_images(id);
        let tab = self.tab_mut();
        tab.doc = doc;
        tab.code.reset();
        tab.live.reset();
        tab.parse.reset(&tab.doc);
        tab.banner = None;
        tab.disk = None;
        tab.find.reset_matches();
        self.error = None;
        self.remember_epoch();
    }

    /// Re-reads the open file. If it's gone, the buffer is kept (it may be
    /// the only copy left) and the missing-file banner says so.
    fn reload(&mut self) {
        let Some(path) = self.tab().doc.path().map(|p| p.to_path_buf()) else {
            return;
        };
        match Document::open(&path) {
            Ok(doc) => self.replace_document(doc),
            Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.tab_mut().banner = Some(Banner::DiskMissing);
                self.tab_mut().disk = Some(Banner::DiskMissing);
            }
            Err(e) => {
                self.error = Some(format!("Couldn't reload {}: {e}", path.display()));
            }
        }
    }

    fn save(&mut self) {
        if self.tabs.is_empty() {
            self.after_save = None;
            return;
        }
        if self.tab().doc.path().is_none() {
            let id = self.tab().id;
            self.spawn_dialog(DialogKind::SaveAs(id));
            return;
        }
        // Never overwrite someone else's changes without asking.
        if matches!(self.tab().doc.disk_status(), Ok(DiskStatus::Modified)) {
            self.tab_mut().banner = Some(Banner::DiskChanged);
            self.tab_mut().disk = Some(Banner::DiskChanged);
            self.after_save = None;
            return;
        }
        match self.tab_mut().doc.save() {
            Ok(()) => self.saved(),
            Err(e) => {
                self.error = Some(format!("Couldn't save: {e}"));
                self.after_save = None;
            }
        }
    }

    fn save_as(&mut self, path: PathBuf) {
        if self.tabs.is_empty() {
            self.after_save = None;
            return;
        }
        // Saving as the open file is a save: don't overwrite changes made
        // by another program without asking.
        let same_file = self
            .tab()
            .doc
            .path()
            .is_some_and(|open| same_file(open, &path));
        if same_file && matches!(self.tab().doc.disk_status(), Ok(DiskStatus::Modified)) {
            self.tab_mut().banner = Some(Banner::DiskChanged);
            self.tab_mut().disk = Some(Banner::DiskChanged);
            self.after_save = None;
            return;
        }
        if self.path_open_elsewhere(&path) {
            self.error = Some(format!(
                "{} is already open in another tab.",
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string())
            ));
            self.after_save = None;
            return;
        }
        match self.tab_mut().doc.save_as(&path) {
            Ok(()) => {
                self.recent.add(&path);
                self.saved();
            }
            Err(e) => {
                self.error = Some(format!("Couldn't save {}: {e}", path.display()));
                self.after_save = None;
            }
        }
    }

    /// Another tab already has `path` open. The tab being saved does not count.
    fn path_open_elsewhere(&self, path: &Path) -> bool {
        let except = self.tabs.get(self.active).map(|tab| tab.id);
        self.tabs.iter().any(|tab| {
            Some(tab.id) != except && tab.doc.path().is_some_and(|open| same_file(open, path))
        })
    }

    fn saved(&mut self) {
        if !self.tabs.is_empty() {
            self.tab_mut().banner = None;
            self.tab_mut().disk = None;
        }
        self.error = None;
        self.touch_session();
        match self.after_save {
            None => {}
            Some(AfterSave::Tab(id)) => {
                self.after_save = None;
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == id)
                    && !self.tabs[index].doc.is_dirty()
                {
                    self.remove_tab(index);
                }
            }
            Some(AfterSave::Window) => {
                if let Some(index) = self.first_dirty_tab() {
                    if index != self.active {
                        self.activate(index);
                    }
                    self.save();
                } else {
                    self.after_save = None;
                    self.close_allowed = true;
                }
            }
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
        let dir = self.tabs.get(self.active).and_then(|tab| {
            tab.doc
                .path()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
        });
        let dir = dir.or_else(|| Some(self.browser.root().to_path_buf()));
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
                DialogKind::SaveAs(id) => DialogResult::SaveAs(
                    id,
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
            Ok(DialogResult::SaveAs(id, Some(path))) => self.finish_save_as(id, path),
            Ok(DialogResult::Folder(Some(path))) => {
                self.browser.set_root(path);
                // The picker can be opened while the sidebar is hidden.
                self.sidebar.visible = true;
                self.sidebar.save();
                self.browser.request_focus();
            }
            Ok(DialogResult::Open(None) | DialogResult::SaveAs(_, None)) => {
                self.after_save = None;
            }
            Ok(DialogResult::MoveTo(path, Some(dir))) => self.move_entry(path, dir),
            Ok(DialogResult::Folder(None) | DialogResult::MoveTo(_, None)) => {}
            Err(TryRecvError::Disconnected) => {
                self.error = Some("The file dialog failed. Is xdg-desktop-portal running?".into());
            }
        }
        self.dialog = None;
    }

    /// Save As finished. The tab that opened the picker may have been
    /// switched away from, or closed, while the dialog was up. A missing
    /// tab writes nothing.
    fn finish_save_as(&mut self, id: u64, path: PathBuf) {
        let Some(index) = self.index_of_tab(id) else {
            self.after_save = None;
            return;
        };
        if index != self.active {
            self.activate(index);
        }
        self.save_as(path);
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
        let code_height = (code_size * CODE_LINE).round();
        let text_height = (text_size * TEXT_LINE).round();
        for tab in &mut self.tabs {
            tab.code.font_size = code_size;
            tab.code.line_height = code_height;
            tab.live.font_size = text_size;
            tab.live.line_height = text_height;
        }
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
            self.poll_tabs_on_disk();
            // The same wake. A caret or scroll move since the tab list was
            // written lands here, without a timer of its own.
            self.remember_session();
        }
        // The same one-second wake as before. A background tab does not
        // ask for a frame: its parse waits until the tab is shown, and the
        // worker does not call back while the tab is asleep.
        ctx.request_repaint_after(DISK_CHECK_INTERVAL);
    }

    /// Every open file, not only the one on screen. A clean background file
    /// with no disk alert is re-read. One already marked missing or changed
    /// keeps the buffer, so a path that comes back does not throw away the
    /// only remaining copy. A dirty one keeps its text and is marked. The
    /// banner appears when that tab is showing.
    fn poll_tabs_on_disk(&mut self) {
        let active = self.active;
        for index in 0..self.tabs.len() {
            let status = self.tabs[index].doc.disk_status();
            let dirty = self.tabs[index].doc.is_dirty();
            match status {
                Ok(DiskStatus::Modified)
                    if !dirty && index != active && self.tabs[index].disk.is_none() =>
                {
                    self.reload_background(index);
                }
                Ok(DiskStatus::Modified) => {
                    self.tabs[index].disk = Some(Banner::DiskChanged);
                    if index == active {
                        self.tabs[index].banner = Some(Banner::DiskChanged);
                    }
                }
                Ok(DiskStatus::Missing) => {
                    self.tabs[index].disk = Some(Banner::DiskMissing);
                    if index == active {
                        self.tabs[index].banner = Some(Banner::DiskMissing);
                    }
                }
                Ok(DiskStatus::Unchanged) => {
                    self.tabs[index].disk = None;
                    if matches!(
                        self.tabs[index].banner,
                        Some(Banner::DiskChanged | Banner::DiskMissing)
                    ) {
                        self.tabs[index].banner = None;
                    }
                }
                Err(_) => {}
            }
        }
    }

    /// Re-reads a tab that is not showing. Its text matched the last save,
    /// so the new bytes replace it. The caret and scroll line stay, clamped
    /// to the new text. A failure leaves the buffer and a mark.
    fn reload_background(&mut self, index: usize) {
        let Some(path) = self.tabs[index].doc.path().map(|path| path.to_path_buf()) else {
            return;
        };
        self.forget_tab_images(self.tabs[index].id);
        let (code_sel, live_sel, code_scroll, live_scroll, folds) = {
            let tab = &self.tabs[index];
            (
                tab.code.selection(),
                tab.live.selection(),
                tab.code.scroll_pos(),
                tab.live.scroll_pos(&tab.doc, tab.parse.output()),
                tab.code.hidden_ranges().to_vec(),
            )
        };
        match Document::open(&path) {
            Ok(doc) => {
                let tab = &mut self.tabs[index];
                tab.doc = doc;
                tab.code.reset();
                tab.live.reset();
                tab.parse.reset(&tab.doc);
                tab.banner = None;
                tab.disk = None;
                tab.find.reset_matches();
                let lines = tab.doc.line_count();
                // mirror, not set_selection: set_selection scrolls to the caret
                // and would throw away the line the view was on.
                tab.code
                    .mirror_selection(clamp_selection(code_sel, &tab.doc));
                tab.live
                    .mirror_selection(clamp_selection(live_sel, &tab.doc));
                tab.code.set_scroll_pos(clamp_scroll(code_scroll, lines));
                tab.code.restore_hidden_ranges(&folds, &tab.doc);
                let pos = clamp_scroll(live_scroll, lines);
                let Tab {
                    doc, parse, live, ..
                } = tab;
                live.set_scroll_pos(doc, parse.output(), pos);
            }
            Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.tabs[index].disk = Some(Banner::DiskMissing);
            }
            Err(_) => {
                self.tabs[index].disk = Some(Banner::DiskChanged);
            }
        }
    }

    fn selection(&self) -> inkmark_buffer::Selection {
        self.tab().selection()
    }

    /// Moves keyboard focus (with the caret and scroll position) to `pane`,
    /// switching away from a single-pane mode that hides it.
    fn focus_pane(&mut self, ctx: &egui::Context, pane: Pane) {
        if self.tabs.is_empty() {
            return;
        }
        let mode_before = self.mode;
        let mut mode = self.mode;
        {
            let tab = self.tab_mut();
            let selection = tab.selection();
            let parse = tab.parse.output();
            match pane {
                Pane::Code => {
                    if mode == Mode::Live {
                        let pos = tab.live.scroll_pos(&tab.doc, parse);
                        tab.code.set_scroll_pos(pos);
                        mode = Mode::Code;
                        // The code pane was hidden, so reveal the caret there.
                        tab.code.set_selection(selection);
                    } else if tab.code.selection() != selection {
                        tab.code.set_selection(selection);
                    }
                    // Same caret again would restart reveal and nudge a settled scroll.
                    tab.code.request_focus(ctx);
                }
                Pane::Live => {
                    if mode == Mode::Code {
                        let pos = tab.code.scroll_pos();
                        tab.live.set_scroll_pos(&tab.doc, parse, pos);
                        mode = Mode::Live;
                        tab.live.set_selection(selection);
                    } else if tab.live.selection() != selection {
                        tab.live.set_selection(selection);
                    }
                    tab.live.request_focus(ctx);
                }
            }
            tab.focus = pane;
        }
        self.mode = mode;
        if self.mode != mode_before {
            self.remember_layout();
        }
    }

    /// Split → code → live → split.
    fn cycle_mode(&mut self, ctx: &egui::Context) {
        if self.tabs.is_empty() {
            return;
        }
        match self.mode {
            Mode::Split => {
                // Pretend live was showing so focus_pane carries its position.
                self.mode = Mode::Live;
                self.focus_pane(ctx, Pane::Code);
            }
            Mode::Code => self.focus_pane(ctx, Pane::Live),
            Mode::Live => {
                // The code pane was hidden: bring it to where live is.
                let pos = {
                    let tab = self.tab_mut();
                    tab.live.scroll_pos(&tab.doc, tab.parse.output())
                };
                self.tab_mut().code.set_scroll_pos(pos);
                self.mode = Mode::Split;
                self.focus_pane(ctx, Pane::Live);
            }
        }
        self.remember_layout();
    }

    /// Writes the mode, both minimaps, and the widths. A resize that only
    /// draws a pane narrower does not call this.
    fn remember_layout(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            self.layout.code_minimap = tab.code.show_minimap;
            self.layout.live_minimap = tab.live.show_minimap;
        }
        self.layout.mode = self.mode;
        self.layout.save();
    }

    /// Ctrl is down when the modifier state says so, or when this frame's
    /// key press carries Ctrl. A headless shortcut is only the key event,
    /// so the event has to count. Releasing Ctrl is a frame with neither.
    fn ctrl_held(ctx: &egui::Context) -> bool {
        ctx.input(|input| {
            if input.modifiers.command || input.modifiers.ctrl {
                return true;
            }
            input.events.iter().any(|event| match event {
                egui::Event::Key {
                    pressed: true,
                    modifiers,
                    ..
                } => modifiers.command || modifiers.ctrl,
                _ => false,
            })
        })
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        // Releasing Ctrl settles a tab walk before this frame's keys run.
        if !Self::ctrl_held(ctx) {
            self.end_mru_walk();
        }
        // The close prompt names one tab. A chord here would switch tabs
        // under that question, and the answer would land on the wrong file.
        if self.confirm.is_some() {
            return;
        }
        // An overlay takes the keyboard. Only its own chord is consumed
        // here, so a key typed into the filter does not also save or switch
        // tabs. Other app chords, including one rebound onto Escape, stay
        // in the event list: Escape still closes the overlay.
        if self.show_keys {
            let toggle = ctx.input_mut(|i| self.keys.consume_action(i, Action::ShowKeys));
            if toggle {
                self.toggle_keys();
            }
            return;
        }
        if self.show_syntax {
            let toggle = ctx.input_mut(|i| self.keys.consume_action(i, Action::ShowSyntax));
            if toggle {
                self.toggle_syntax();
            }
            return;
        }
        // App chords are taken before a pane sees the key. Matching is
        // exact, so a pane chord that adds Shift or Alt (Ctrl+Alt+1 is a
        // heading, Alt+Shift+Left moves a table column) is left for the pane.
        let actions = ctx.input_mut(|i| self.keys.consume(i, Scope::App));
        for action in actions {
            self.run_app_action(ctx, action);
        }
    }

    fn run_app_action(&mut self, ctx: &egui::Context, action: Action) {
        // With no document, only opening, the sidebar, and the key list run.
        // A new action is blocked until it is named here, so it cannot call
        // `tab()` on an empty window.
        if self.tabs.is_empty()
            && !matches!(
                action,
                Action::OpenFile
                    | Action::OpenFolder
                    | Action::NewFile
                    | Action::NewFolder
                    | Action::RecentFiles
                    | Action::ToggleSidebar
                    | Action::ToggleOutline
                    | Action::SearchFolder
                    | Action::ShowKeys
                    | Action::ShowSyntax
                    | Action::ReopenTab
            )
        {
            return;
        }
        match action {
            Action::ToggleSidebar => self.toggle_sidebar(ctx),
            Action::ToggleOutline => {
                self.layout.outline_visible = !self.layout.outline_visible;
                self.remember_layout();
            }
            Action::RecentFiles => {
                self.show_keys = false;
                self.show_syntax = false;
                if self.tabs.is_empty() {
                    // The pane already shows the list. Ctrl+R only clears a
                    // dialog that was covering it.
                    self.new_file = None;
                    self.new_folder = None;
                    self.recent_list = None;
                } else {
                    self.recent_list = match self.recent_list {
                        Some(_) => None,
                        None => {
                            self.new_file = None;
                            self.new_folder = None;
                            Some(0)
                        }
                    };
                }
            }
            Action::ToggleMinimap => {
                // The code pane and the live pane each keep their own
                // minimap. The choice is the window's, so every tab follows.
                let focus = self.tab().focus;
                let on = match focus {
                    Pane::Code => !self.tab().code.show_minimap,
                    Pane::Live => !self.tab().live.show_minimap,
                };
                for tab in &mut self.tabs {
                    match focus {
                        Pane::Code => tab.code.show_minimap = on,
                        Pane::Live => tab.live.show_minimap = on,
                    }
                }
                self.remember_layout();
            }
            Action::CycleMode => self.cycle_mode(ctx),
            Action::FocusCode => self.focus_pane(ctx, Pane::Code),
            Action::FocusLive => self.focus_pane(ctx, Pane::Live),
            Action::Back => self.go_back(),
            Action::Forward => self.go_forward(),
            Action::OpenFolder => self.spawn_dialog(DialogKind::Folder),
            Action::SaveAs => {
                let id = self.tab().id;
                self.spawn_dialog(DialogKind::SaveAs(id));
            }
            Action::Save => self.save(),
            Action::OpenFile => self.spawn_dialog(DialogKind::Open),
            Action::CloseTab => self.close_tab(self.active),
            Action::ReopenTab => self.reopen_closed(),
            Action::TabNext => self.cycle_mru(true),
            Action::TabPrevious => self.cycle_mru(false),
            Action::TabLeft => self.move_tab(-1),
            Action::TabRight => self.move_tab(1),
            Action::Tab1 => self.jump_tab(0),
            Action::Tab2 => self.jump_tab(1),
            Action::Tab3 => self.jump_tab(2),
            Action::Tab4 => self.jump_tab(3),
            Action::Tab5 => self.jump_tab(4),
            Action::Tab6 => self.jump_tab(5),
            Action::Tab7 => self.jump_tab(6),
            Action::Tab8 => self.jump_tab(7),
            Action::Tab9 => self.jump_tab(8),
            Action::NewFile => self.begin_new_file(),
            Action::NewFolder => self.begin_new_folder(),
            Action::ShowKeys => self.toggle_keys(),
            Action::ShowSyntax => self.toggle_syntax(),
            Action::Find => self.open_find(ctx, false),
            Action::Replace => self.open_find(ctx, true),
            Action::FindNext => self.find_move(true),
            Action::FindPrevious => self.find_move(false),
            Action::GoToLine => self.open_goto(ctx),
            Action::SearchFolder => self.open_folder_search(ctx),
            // The search field reads this chord itself. It is not an app chord.
            Action::PinSearchResult => {}
            _ => {}
        }
    }

    /// Ctrl+F / Ctrl+H. A dialog already owns the keyboard, so find waits.
    fn open_find(&mut self, ctx: &egui::Context, replace: bool) {
        if self.tabs.is_empty() || self.modal_open() {
            return;
        }
        self.goto.close();
        self.search.close();
        let selection = self.selection();
        let range = selection.range();
        let already_open = self.find_open;
        let seed = FindState::seed(self.tab().doc.slice(range.clone()).as_ref());
        {
            let tab = self.tab_mut();
            tab.find.open(range.start, seed, replace, already_open);
            // The bar takes the keys; the caret comes back when it closes.
            tab.code.release_focus(ctx);
            tab.live.release_focus(ctx);
        }
        self.find_open = true;
    }

    /// Ctrl+G. A dialog already owns the keyboard, so the prompt waits.
    fn open_goto(&mut self, ctx: &egui::Context) {
        if self.tabs.is_empty() || self.modal_open() {
            return;
        }
        self.find_open = false;
        self.search.close();
        self.goto.open();
        self.tab_mut().code.release_focus(ctx);
        self.tab_mut().live.release_focus(ctx);
    }

    /// Ctrl+Shift+F. A hidden sidebar is shown, and the query takes the keys.
    fn open_folder_search(&mut self, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }
        self.find_open = false;
        self.goto.close();
        if !self.sidebar.visible {
            self.sidebar.visible = true;
            self.sidebar.save();
        }
        self.search.open();
        if !self.tabs.is_empty() {
            self.tab_mut().code.release_focus(ctx);
            self.tab_mut().live.release_focus(ctx);
        }
    }

    fn find_move(&mut self, next: bool) {
        if self.tabs.is_empty() {
            return;
        }
        let selection = self.selection();
        let moved = {
            let tab = self.tab_mut();
            if next {
                tab.find.goto_next(&tab.doc, selection)
            } else {
                tab.find.goto_prev(&tab.doc, selection)
            }
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
        } else if let Some(tab) = self.tabs.get(self.active) {
            let focus = tab.focus;
            self.focus_pane(ctx, focus);
        }
    }

    /// Ctrl+/ or F1 opens the list, and either key closes it. Another dialog
    /// keeps it shut.
    fn toggle_keys(&mut self) {
        if self.show_keys {
            self.close_keys();
            return;
        }
        if self.modal_open() {
            return;
        }
        self.recent_list = None;
        self.keys_query.clear();
        self.keys_focus_filter = true;
        self.show_keys = true;
    }

    fn close_keys(&mut self) {
        self.show_keys = false;
        self.keys_focus_filter = false;
        self.show_recent_if_empty();
        // The panes draw after this, under the modal layer from the previous
        // frame, and would surrender a focus request made now.
        // `finish_keys_focus` asks after they draw.
    }

    /// Ctrl+Shift+/ opens the cheatsheet, and the same chord closes it.
    /// Another dialog keeps it shut. Escape closes it from `syntax_ui`.
    fn toggle_syntax(&mut self) {
        if self.show_syntax {
            self.close_syntax();
            return;
        }
        if self.modal_open() {
            return;
        }
        self.recent_list = None;
        self.show_syntax = true;
    }

    fn close_syntax(&mut self) {
        self.show_syntax = false;
        self.show_recent_if_empty();
        // Same as `close_keys`: the panes would surrender a focus request
        // made before they draw. `finish_keys_focus` asks after they draw.
    }

    /// The key list or the syntax cheatsheet is up.
    fn overlay_open(&self) -> bool {
        self.show_keys || self.show_syntax
    }

    /// Puts keyboard focus back on the code or live pane that had it.
    ///
    /// `request_focus` clears the arrow lock. The lock sticks on the
    /// following frame, once the pane already had focus at the start of
    /// that frame (CRO-128). [`Self::finish_keys_focus`] arms that frame.
    fn restore_editor_focus(&mut self, ctx: &egui::Context) {
        if self.tabs.is_empty() {
            return;
        }
        match self.tab().focus {
            Pane::Code => self.tab().code.request_focus(ctx),
            Pane::Live => self.tab().live.request_focus(ctx),
        }
    }

    /// After the panes have drawn, on the frame an overlay closes: focus
    /// the pane it borrowed. The filter or the cheatsheet keeps focus while
    /// it is open. The arrow lock cannot stick on this same frame, because the
    /// pane did not have focus when the frame began, so the next frame arms
    /// it (CRO-128).
    fn finish_keys_focus(&mut self, ctx: &egui::Context) {
        if self.keys_arm_lock {
            self.keys_arm_lock = false;
            // begin_pass already saw the unlocked arrow. Drop that focus
            // move so the key edits the pane and the pane keeps the keyboard.
            ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
        }
        let closing = self.keys_return_focus && !self.overlay_open();
        self.keys_return_focus = self.overlay_open();
        // A new-file or rename prompt can replace the list on this frame.
        // That prompt owns the keyboard.
        if !closing || self.modal_open() {
            return;
        }
        self.restore_editor_focus(ctx);
        self.keys_arm_lock = true;
    }

    fn modal_open(&self) -> bool {
        self.recent_list.is_some()
            || self.confirm.is_some()
            || self.new_file.is_some()
            || self.new_folder.is_some()
            || self.overlay_open()
            || self.rename.is_some()
            || self.trash_confirm.is_some()
            || self.dialog.is_some()
    }

    /// Ctrl+Z and Ctrl+Y while the bar holds the keyboard. A field would
    /// undo its own text, and a button click leaves the panes unfocused.
    fn find_history_keys(&mut self, ctx: &egui::Context) {
        if self.tabs.is_empty() || !self.find_open || self.modal_open() {
            return;
        }
        let (field, pane) = {
            let tab = self.tab();
            (
                tab.find.field_focused(ctx),
                tab.code.has_focus(ctx) || tab.live.has_focus(ctx),
            )
        };
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
                Action::Undo => self.tab_mut().doc.undo(),
                Action::Redo => self.tab_mut().doc.redo(),
                _ => None,
            };
            if let Some(selection) = restored {
                self.show_match(selection);
                let tab = self.tab_mut();
                tab.find.sync_count(&tab.doc, selection);
            }
        }
    }

    /// Selects `selection` in the focused pane and mirrors it to the other.
    fn show_match(&mut self, selection: Selection) {
        self.show_match_in(self.active, selection);
    }

    fn show_match_in(&mut self, index: usize, selection: Selection) {
        let tab = &mut self.tabs[index];
        match tab.focus {
            Pane::Code => {
                tab.code.set_selection(selection);
                tab.live.mirror_selection(selection);
            }
            Pane::Live => {
                tab.live.set_selection(selection);
                tab.code.mirror_selection(selection);
            }
        }
    }

    /// The first unsaved tab in strip order, if there is one.
    fn first_dirty_tab(&self) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.doc.is_dirty())
    }

    fn dirty_count(&self) -> usize {
        self.tabs.iter().filter(|tab| tab.doc.is_dirty()).count()
    }

    fn dirty_labels(&self) -> Vec<String> {
        self.tabs
            .iter()
            .filter(|tab| tab.doc.is_dirty())
            .map(|tab| {
                tab.doc
                    .path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "untitled".to_owned())
            })
            .collect()
    }

    /// One unsaved tab uses the single prompt. Several unsaved tabs use
    /// one list. Discard closes the window either way.
    fn guard_close(&mut self, ctx: &egui::Context) {
        let requested = ctx.input(|i| i.viewport().close_requested());
        if requested {
            // The one-second check may not have run since the last caret move.
            self.remember_session();
        }
        let dirty = self.dirty_count();
        if requested && !self.logged_close {
            let cancel = dirty > 0 && !self.close_allowed;
            session::line(&format!("close_requested dirty={dirty} cancel={cancel}"));
            self.logged_close = true;
        }
        if !requested {
            self.logged_close = false;
        }
        if requested && !self.close_allowed && dirty > 0 {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if dirty == 1
                && let Some(index) = self.first_dirty_tab()
            {
                if index != self.active {
                    self.activate(index);
                }
                let id = self.tabs[index].id;
                self.confirm = Some(Confirm::Close(id));
            } else {
                self.confirm = Some(Confirm::CloseAll);
            }
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
        let (path, dirty) = match self.tabs.get(self.active) {
            Some(tab) => (
                tab.doc
                    .path()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "untitled".to_owned()),
                tab.doc.is_dirty(),
            ),
            None => ("none".to_owned(), false),
        };
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
                "alive focused={focused} dirty={dirty} path={path}"
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
        self.tab().title()
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let title = if self.tabs.is_empty() {
            "inkmark".to_owned()
        } else {
            let dirty = if self.tab().doc.is_dirty() {
                "● "
            } else {
                ""
            };
            format!("{dirty}{} — inkmark", self.file_name())
        };
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
        if self.tabs.is_empty() {
            if action == Some("dismiss") {
                self.error = None;
            }
            return;
        }
        let Some(banner) = &self.tab().banner else {
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
                self.tab_mut().banner = None;
                self.tab_mut().disk = None;
                if let Err(e) = self.tab_mut().doc.acknowledge_disk_state() {
                    self.error = Some(format!("Couldn't check the file: {e}"));
                }
            }
            _ => {}
        }
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        if self.tabs.is_empty() {
            self.status_empty(ui);
            return;
        }
        let head = self.selection().head;
        let (line, column, encoding, path, dirty) = {
            let doc = &self.tab().doc;
            let line = doc.byte_to_line(head);
            let column = doc.slice(doc.line_to_byte(line)..head).chars().count() + 1;
            let path = doc
                .path()
                .map_or_else(|| "untitled".into(), |p| p.display().to_string());
            (line, column, doc.encoding(), path, doc.is_dirty())
        };
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
            ui.label(&path);
            if dirty {
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

    /// Status bar with no document: the sidebar and outline toggles stay.
    fn status_empty(&mut self, ui: &mut egui::Ui) {
        let files_tip = binding_tip(&self.keys, Action::ToggleSidebar);
        let outline_tip = binding_tip(&self.keys, Action::ToggleOutline);
        let mut toggle_files = false;
        let mut toggle_outline = false;
        ui.horizontal(|ui| {
            let files = ui
                .add(egui::Button::new("«").small())
                .on_hover_text(&files_tip);
            if files.clicked() {
                toggle_files = true;
            }
            ui.label("No file open");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let outline = ui
                    .add(egui::Button::new("»").small())
                    .on_hover_text(&outline_tip);
                if outline.clicked() {
                    toggle_outline = true;
                }
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
            || self.overlay_open()
            || self.rename.is_some()
            || self.trash_confirm.is_some();
        if self.tabs.is_empty() {
            if open {
                ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("file_browser")));
            }
            self.modal_was_open = open;
            return;
        }
        // The filter or the cheatsheet holds the keyboard while that overlay
        // is open. Releasing the browser keeps the sidebar from taking keys.
        // The editor is focused again when the overlay closes (CRO-128).
        if self.overlay_open() {
            ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("file_browser")));
            self.modal_was_open = true;
            return;
        }
        if open {
            self.tab_mut().code.release_focus(ctx);
            self.tab_mut().live.release_focus(ctx);
            ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("file_browser")));
        } else if self.modal_was_open {
            self.restore_editor_focus(ctx);
        }
        self.modal_was_open = open;
    }

    /// The recent-files dialog, while a document is open. An empty window
    /// draws the same rows in the pane instead, so this stays closed.
    fn recent_ui(&mut self, ctx: &egui::Context) {
        if self.tabs.is_empty() {
            self.recent_list = None;
            return;
        }
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
            if let Some(path) = Self::recent_rows(ui, &entries, selected, false) {
                chosen = Some(path);
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

    /// Recent-file rows. The clicked path, when a row was clicked.
    /// `reveal` scrolls the selected row into view. The modal list passes
    /// false; it has no scroll parent.
    fn recent_rows(
        ui: &mut egui::Ui,
        entries: &[PathBuf],
        selected: usize,
        reveal: bool,
    ) -> Option<PathBuf> {
        let mut chosen = None;
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
            if reveal && i == selected {
                row.scroll_to_me(Some(egui::Align::Center));
            }
            ui.label(RichText::new(dir).small().weak());
            if row.clicked() {
                chosen = Some(path.clone());
            }
        }
        chosen
    }

    /// Recent files in the empty pane, where a document would be. Not a
    /// dialog: the sidebar beside it still receives clicks. With nothing
    /// recent, Open and New are the way back to a document.
    fn empty_pane(&mut self, ui: &mut egui::Ui) {
        let entries = self.recent.entries().to_vec();
        if entries.is_empty() {
            self.empty_recent = 0;
        } else if self.empty_recent >= entries.len() {
            self.empty_recent = entries.len() - 1;
        }
        let ctx = ui.ctx().clone();
        let keys = !self.modal_open() && !self.search.is_open() && !self.browser.has_focus(&ctx);
        let mut open_path = None;
        let mut reveal = false;
        if keys && !entries.is_empty() {
            let (up, down, enter) = ui.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(Modifiers::NONE, Key::Enter),
                )
            });
            if up {
                self.empty_recent = self.empty_recent.saturating_sub(1);
                reveal = true;
            }
            if down {
                self.empty_recent = (self.empty_recent + 1).min(entries.len() - 1);
                reveal = true;
            }
            if enter {
                open_path = Some(entries[self.empty_recent].clone());
            }
        }
        let selected = self.empty_recent;
        let mut chosen = None;
        let mut open_file = false;
        let mut new_file = false;
        ui.add_space(24.0);
        ui.horizontal(|ui| {
            ui.add_space(24.0);
            ui.vertical(|ui| {
                ui.heading("Recent files");
                ui.add_space(6.0);
                if entries.is_empty() {
                    ui.label("No recent files");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Open…").clicked() {
                            open_file = true;
                        }
                        if ui.button("New").clicked() {
                            new_file = true;
                        }
                    });
                } else {
                    // Ten entries are two rows each. A short window clips the
                    // lower ones unless this list scrolls and follows the
                    // selection. The hint stays put, under the list.
                    let hint = 28.0;
                    let height = (ui.max_rect().bottom() - ui.cursor().min.y - hint).max(40.0);
                    egui::ScrollArea::vertical()
                        .id_salt("empty_recent")
                        .auto_shrink([false, false])
                        .animated(false)
                        .max_height(height)
                        .show(ui, |ui| {
                            chosen = Self::recent_rows(ui, &entries, selected, reveal);
                        });
                    ui.add_space(6.0);
                    ui.label(RichText::new("Up/Down select · Enter open").small().weak());
                }
            });
        });
        if let Some(path) = open_path.or(chosen) {
            self.request_open(path);
        } else if open_file {
            self.spawn_dialog(DialogKind::Open);
        } else if new_file {
            self.begin_new_file();
        }
    }

    fn confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm.clone() else {
            return;
        };
        let all = matches!(confirm, Confirm::CloseAll);
        let labels = if all { self.dirty_labels() } else { Vec::new() };
        let name = match &confirm {
            Confirm::CloseAll => String::new(),
            Confirm::Close(id) | Confirm::CloseTab(id) => {
                let id = *id;
                self.tabs
                    .iter()
                    .find(|tab| tab.id == id)
                    .map(|tab| tab.title())
                    .unwrap_or_else(|| "untitled".to_owned())
            }
        };
        let mut choice = None;
        egui::Modal::new(egui::Id::new("confirm")).show(ctx, |ui| {
            if all {
                ui.label("These files have unsaved changes:");
                for label in &labels {
                    ui.label(label);
                }
            } else {
                ui.label(format!("{name} has unsaved changes."));
            }
            ui.horizontal(|ui| {
                let save = if all { "Save all" } else { "Save" };
                let discard = if all { "Discard all" } else { "Discard" };
                if ui.button(save).clicked() {
                    choice = Some("save");
                }
                if ui.button(discard).clicked() {
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
            ("save", Confirm::Close(id)) => {
                let Some(index) = self.index_of_tab(id) else {
                    return;
                };
                if index != self.active {
                    self.activate(index);
                }
                self.after_save = Some(AfterSave::Window);
                self.save();
            }
            ("save", Confirm::CloseTab(id)) => {
                let Some(index) = self.index_of_tab(id) else {
                    return;
                };
                if index != self.active {
                    self.activate(index);
                }
                self.after_save = Some(AfterSave::Tab(id));
                self.save();
            }
            ("save", Confirm::CloseAll) => {
                self.after_save = Some(AfterSave::Window);
                if let Some(index) = self.first_dirty_tab() {
                    if index != self.active {
                        self.activate(index);
                    }
                    self.save();
                } else {
                    self.after_save = None;
                    self.close_allowed = true;
                }
            }
            ("discard", Confirm::Close(_) | Confirm::CloseAll) => self.close_allowed = true,
            ("discard", Confirm::CloseTab(id)) => {
                if let Some(index) = self.index_of_tab(id) {
                    self.remove_tab(index);
                }
            }
            ("cancel", _) => self.after_save = None,
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
        self.show_syntax = false;
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
        self.show_syntax = false;
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
            self.show_recent_if_empty();
            return;
        }
        self.new_folder = Some(prompt);
        if submit {
            self.submit_new_folder();
        }
    }

    /// The chords in effect, grouped and filtered. Escape, Ctrl+/, or F1
    /// closes the list. The filter is focused when the list opens, and it
    /// keeps that focus until the list closes.
    fn keys_ui(&mut self, ctx: &egui::Context) {
        if !self.show_keys {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.close_keys();
            return;
        }
        let field = egui::Id::new("keys_filter");
        let os = ctx.os();
        // Stay inside a small window. 640×420 is the preferred size; the
        // e2e viewport is 480×320, and a fixed minimum overflows it.
        let screen = ctx.content_rect();
        let pad = 32.0;
        let max_w = (screen.width() - pad * 2.0).max(200.0);
        let max_h = (screen.height() - pad * 2.0).max(160.0);
        // Preferred size, but never wider or taller than this window. The
        // e2e viewport is 480×320.
        let width = max_w.min(640.0);
        let list_h = (max_h - 140.0).clamp(80.0, 420.0);
        egui::Modal::new(egui::Id::new("keys")).show(ctx, |ui| {
            ui.set_width(width);
            ui.heading("Key bindings");
            ui.add_space(4.0);
            // The modal's first frame only measures, and a focus request
            // there is dropped. Ask once, on the frame that can hold it.
            // Asking again later interrupts IME composition.
            if self.keys_focus_filter && !ui.is_sizing_pass() {
                self.keys_focus_filter = false;
                ui.memory_mut(|m| m.request_focus(field));
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.keys_query)
                    .id(field)
                    .hint_text("Filter by name or key")
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);
            let rows = self.overlay_rows(os);
            // A stable height, capped by the window. The modal keeps whatever
            // size it had last frame, so a list that shrank to one filter
            // result would clip the next, taller result.
            let scroll_width = ui.available_width();
            ui.allocate_ui(egui::vec2(scroll_width, list_h), |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if rows.is_empty() {
                        ui.label("No matching shortcuts");
                        return;
                    }
                    let mut shown: Option<keys::Group> = None;
                    for (group, chord, description) in &rows {
                        if shown != Some(*group) {
                            if shown.is_some() {
                                ui.add_space(8.0);
                            }
                            ui.label(RichText::new(group.label()).strong());
                            shown = Some(*group);
                        }
                        // Wrap, so a long row cannot stretch the dialog
                        // past the window.
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(chord).strong());
                            ui.add_space(16.0);
                            ui.label(description);
                        });
                    }
                });
            });
            ui.add_space(6.0);
            let close = self.keys.platform_text(os, Action::ShowKeys);
            ui.label(
                RichText::new(format!("Esc or {close} closes"))
                    .small()
                    .weak(),
            );
        });
        // The filter read these. The pane must not apply them too.
        self.eat_overlay_keys(ctx);
    }

    /// One row of the overlay: group, chord text for this machine, description.
    ///
    /// Actions come from [`keys::KeyMap::bindings`], the same rows input
    /// handling reads. Pointer gestures follow the actions in their group.
    fn overlay_rows(&self, os: egui::os::OperatingSystem) -> Vec<(keys::Group, String, String)> {
        let query = self.keys_query.trim().to_lowercase();
        let mut rows = Vec::new();
        for group in keys::Group::ALL {
            for action in Action::ALL {
                let binding_group = self.keys.bindings()[action.index()].group;
                let description = self.keys.bindings()[action.index()].description;
                if binding_group != group || !self.keys.matches_lower(action, os, &query) {
                    continue;
                }
                let chord = self.keys.platform_text(os, action);
                let chord = if chord.is_empty() {
                    "unbound".to_owned()
                } else {
                    chord
                };
                rows.push((group, chord, description.to_owned()));
            }
            for gesture in keys::GESTURES {
                if gesture.group == group && gesture.matches_lower(&query) {
                    rows.push((
                        group,
                        gesture.platform_keys(os),
                        gesture.description.to_owned(),
                    ));
                }
            }
        }
        rows
    }

    /// Drops key and text events the filter already applied, so the pane
    /// behind the overlay does not type or move.
    fn eat_overlay_keys(&self, ctx: &egui::Context) {
        ctx.input_mut(|input| {
            input.events.retain(|event| {
                !matches!(
                    event,
                    egui::Event::Key { .. }
                        | egui::Event::Text(_)
                        | egui::Event::Paste(_)
                        | egui::Event::Copy
                        | egui::Event::Cut
                        | egui::Event::Ime(_)
                )
            });
        });
    }

    /// The Markdown cheatsheet. Each row is the source in the code font and
    /// the same source through the live renderer. Escape or Ctrl+Shift+/
    /// closes it. Clicking a row inserts that snippet at the caret.
    fn syntax_ui(&mut self, ctx: &egui::Context) {
        if !self.show_syntax {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.close_syntax();
            return;
        }
        self.sync_syntax_type();
        if let Some(cache) = self.images.clone() {
            self.syntax.share_images(cache, u64::MAX);
        }
        self.syntax.begin_rows();
        let screen = ctx.content_rect();
        let pad = 32.0;
        let max_w = (screen.width() - pad * 2.0).max(200.0);
        let max_h = (screen.height() - pad * 2.0).max(160.0);
        // Preferred size, but never wider or taller than this window. The
        // e2e viewport is 480×320.
        let width = max_w.min(760.0);
        let list_h = (max_h - 160.0).clamp(80.0, 480.0);
        let close = self.keys.platform_text(ctx.os(), Action::ShowSyntax);
        let sheet = egui::Id::new("syntax_sheet");
        let mut clicked = None;
        egui::Modal::new(egui::Id::new("syntax")).show(ctx, |ui| {
            ui.set_width(width);
            // A focus target on this layer, so the pane underneath gives up
            // the keyboard. Registered before the rows, so a row click stays
            // on top. Asking again each frame would reset that focus.
            let _focus = ui.interact(
                egui::Rect::from_min_size(ui.cursor().min, egui::vec2(1.0, 1.0)),
                sheet,
                egui::Sense::click(),
            );
            if !ui.is_sizing_pass() && !ui.memory(|m| m.has_focus(sheet)) {
                ui.memory_mut(|m| m.request_focus(sheet));
            }
            ui.heading("Markdown syntax");
            ui.add_space(4.0);
            ui.columns_const(|[left, right]| {
                left.label(RichText::new("Syntax").strong());
                right.label(RichText::new("Rendered").strong());
            });
            ui.add_space(4.0);
            let scroll_width = ui.available_width();
            ui.allocate_ui(egui::vec2(scroll_width, list_h), |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (index, sample) in inkmark_view::samples().iter().enumerate() {
                        let doc = Document::from_text(&sample.source);
                        let parse = GfmParser.parse(&sample.source);
                        if self
                            .syntax
                            .show_row(ui, index, &sample.source, &doc, &parse)
                        {
                            clicked = Some(index);
                        }
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Esc or {close} closes. Click a row to insert it."))
                    .small()
                    .weak(),
            );
        });
        if let Some(index) = clicked {
            let source = inkmark_view::samples()[index].source.clone();
            self.insert_snippet(ctx, &source);
        }
        self.eat_overlay_keys(ctx);
    }

    /// Font sizes from the open tab, so the two columns match the panes.
    fn sync_syntax_type(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            self.syntax.set_type_size(
                tab.code.font_size,
                tab.code.line_height,
                tab.live.font_size,
                tab.live.line_height,
            );
            return;
        }
        let code_size = self.settings.code_size.unwrap_or(CODE_SIZE);
        let text_size = self.settings.text_size.unwrap_or(TEXT_SIZE);
        self.syntax.set_type_size(
            code_size,
            (code_size * CODE_LINE).round(),
            text_size,
            (text_size * TEXT_LINE).round(),
        );
    }

    /// Puts `source` at the caret. With no note open, copies it instead.
    /// The cheatsheet stays open.
    fn insert_snippet(&mut self, ctx: &egui::Context, source: &str) {
        if self.tabs.is_empty() {
            ctx.copy_text(source.to_owned());
            self.show_hint("Copied the snippet");
            return;
        }
        let before = self.selection();
        let range = before.range();
        let after = Selection::caret(range.start + source.len());
        if self
            .tab_mut()
            .doc
            .apply(
                vec![inkmark_buffer::Edit::replace(range, source)],
                before,
                after,
                inkmark_buffer::EditKind::Other,
            )
            .is_ok()
        {
            self.show_match(after);
        }
    }

    /// Creates the named file and opens it in a tab. The empty file stays
    /// on disk even if a later close of that tab is refused.
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
            self.show_recent_if_empty();
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
            || self.overlay_open()
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
            self.show_recent_if_empty();
            return;
        }
        self.rename = Some(prompt.clone());
        if !submit {
            return;
        }
        let followed = self.tabs_under(&prompt.path);
        match rename(&prompt.path, &prompt.name) {
            Ok(new) => {
                self.rename = None;
                self.moved(&prompt.path, &new, &followed);
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
        let followed = self.tabs_under(&path);
        match move_into(&path, &dir) {
            Ok(new) if new == path => {}
            Ok(new) => self.moved(&path, &new, &followed),
            Err(e) => {
                self.error = Some(format!("Couldn't move {}: {e}", display_name(&path)));
            }
        }
    }

    /// Every tab whose file is `path` or inside it, and whether that tab
    /// matched the disk just now. Asked before a rename or move changes
    /// ctime. The bool is false when the disk copy already differs.
    fn tabs_under(&self, path: &Path) -> Vec<(usize, bool)> {
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| {
                let open = tab.doc.path()?;
                if !open.starts_with(path) {
                    return None;
                }
                let unchanged = matches!(tab.doc.disk_status(), Ok(DiskStatus::Unchanged));
                Some((index, unchanged))
            })
            .collect()
    }

    /// `old` became `new`: every tab on that path, the recent list, and
    /// the sidebar follow. `followed` is the snapshot from before the
    /// rename or move.
    fn moved(&mut self, old: &Path, new: &Path, followed: &[(usize, bool)]) {
        let relocate = |path: &Path| -> Option<PathBuf> {
            let rest = path.strip_prefix(old).ok()?;
            Some(if rest.as_os_str().is_empty() {
                new.to_path_buf()
            } else {
                new.join(rest)
            })
        };
        let mut session_moved = false;
        for &(index, was_unchanged) in followed {
            let Some(open) = self.tabs[index].doc.path().map(Path::to_path_buf) else {
                continue;
            };
            if let Some(path) = relocate(&open) {
                self.tabs[index].doc.moved_to(path, was_unchanged);
                session_moved = true;
            }
        }
        // Places to go back to, and a jump waiting on a file, follow too.
        for place in self.back.iter_mut().chain(self.forward.iter_mut()) {
            if let Some(path) = &mut place.path
                && let Some(updated) = relocate(path)
            {
                *path = updated;
            }
        }
        if let Some(pending) = &mut self.pending_jump
            && let Some(updated) = relocate(&pending.path)
        {
            pending.path = updated;
        }
        if let Some(pending) = &mut self.pending_history {
            if let Some(updated) = relocate(&pending.path) {
                pending.path = updated;
            }
            if let Some(path) = pending.here.path.as_mut()
                && let Some(updated) = relocate(path)
            {
                *path = updated;
            }
        }
        self.recent.moved(old, new);
        for closed in &mut self.closed {
            if let Some(updated) = relocate(&closed.path) {
                closed.path = updated;
                session_moved = true;
            }
        }
        if session_moved {
            self.touch_session();
        }
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
        let open_inside = self
            .tabs
            .iter()
            .any(|tab| tab.doc.path().is_some_and(|open| open.starts_with(&path)));
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
            self.show_recent_if_empty();
        } else if confirm {
            self.trash_confirm = None;
            match self.trash.trash(&path) {
                Ok(()) => {
                    if let Some(dir) = path.parent() {
                        self.browser.refresh_dir(dir);
                    }
                    if open_inside {
                        for tab in &mut self.tabs {
                            if tab.doc.path().is_some_and(|open| open.starts_with(&path)) {
                                tab.banner = Some(Banner::DiskMissing);
                                tab.disk = Some(Banner::DiskMissing);
                            }
                        }
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
        let open_path = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.doc.path().map(|path| path.to_path_buf()));
        let mut dirty_paths = HashSet::new();
        for tab in &self.tabs {
            if tab.doc.is_dirty()
                && let Some(path) = tab.doc.path()
            {
                dirty_paths.insert(path.to_path_buf());
            }
        }
        let mut changed_paths = HashSet::new();
        let mut missing_paths = HashSet::new();
        for tab in &self.tabs {
            if let Some(path) = tab.doc.path() {
                match tab.disk {
                    Some(Banner::DiskChanged) => {
                        changed_paths.insert(path.to_path_buf());
                    }
                    Some(Banner::DiskMissing) => {
                        missing_paths.insert(path.to_path_buf());
                    }
                    None => {}
                }
            }
        }
        self.browser.set_current(open_path);
        self.browser.set_dirty(&dirty_paths);
        self.browser.set_changed(&changed_paths);
        self.browser.set_missing(&missing_paths);
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
            // The bar reads the live table. Clone so the sidebar can borrow
            // `search` mutably while it still has the chords.
            let search_keys = self.search.is_open().then(|| self.keys.clone());
            let (hit, closed) = ui
                .scope_builder(UiBuilder::new().max_rect(left), |ui| {
                    let bar = if let Some(keys) = &search_keys {
                        Some(self.search.show_bar(ui, &root, show_all, modal, keys))
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
                if let Some(tab) = self.tabs.get(self.active) {
                    let focus = tab.focus;
                    self.focus_pane(ui.ctx(), focus);
                }
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
        if panes.width() > 1.0 && self.tabs.is_empty() {
            ui.scope_builder(UiBuilder::new().max_rect(panes), |ui| {
                self.empty_pane(ui);
            });
        } else if panes.width() > 1.0 {
            let mut tab_events = Vec::new();
            ui.scope_builder(UiBuilder::new().max_rect(panes), |ui| {
                if self.tabs.len() > 1 {
                    let full = ui.max_rect();
                    let strip = Rect::from_min_max(
                        full.min,
                        pos2(full.right(), (full.top() + tabs::HEIGHT).min(full.bottom())),
                    );
                    let body = Rect::from_min_max(pos2(full.left(), strip.bottom()), full.max);
                    let items = self.tab_strip_items();
                    tab_events = ui
                        .scope_builder(UiBuilder::new().max_rect(strip), |ui| {
                            tabs::show(ui, &items)
                        })
                        .inner;
                    if body.height() > 1.0 {
                        ui.scope_builder(UiBuilder::new().max_rect(body), |ui| self.panes(ui));
                    }
                } else {
                    self.panes(ui);
                }
            });
            self.apply_tab_events(tab_events);
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
            if self.tabs.is_empty() {
                self.outline.clear();
            } else {
                // Field borrow: `tab()` would take all of `self`, and the
                // outline is borrowed beside the document.
                let tab = &self.tabs[self.active];
                self.outline
                    .refresh(tab.parse.revision(), &tab.doc, tab.parse.output());
            }
            let caret = self
                .tabs
                .get(self.active)
                .map(|tab| tab.selection().head)
                .unwrap_or(0);
            let clicked = ui
                .scope_builder(UiBuilder::new().max_rect(outline_rect), |ui| {
                    outline::show(ui, self.outline.rows(), caret)
                })
                .inner;
            if let Some(offset) = clicked
                && !self.tabs.is_empty()
            {
                let here = self.here();
                self.jump_to(offset);
                self.remember(here);
                let focus = self.tab().focus;
                self.focus_pane(ui.ctx(), focus);
            }
        }
    }

    fn tab_strip_items(&self) -> Vec<tabs::Item> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| tabs::Item {
                id: tab.id,
                title: tab.title(),
                dirty: tab.doc.is_dirty(),
                changed: matches!(tab.disk, Some(Banner::DiskChanged)),
                missing: matches!(tab.disk, Some(Banner::DiskMissing)),
                active: index == self.active,
                preview: tab.preview,
            })
            .collect()
    }

    fn apply_tab_events(&mut self, events: Vec<tabs::Event>) {
        for event in events {
            match event {
                tabs::Event::Activate(index) => self.activate(index),
                tabs::Event::Close(index) => self.close_tab(index),
            }
        }
    }

    /// A folder-search row. The same file selects the match; another file
    /// opens in the preview tab, or a pinned tab when `pin` is set. The
    /// match is selected in the tab that open lands in.
    fn open_search_hit(&mut self, hit: SearchChoice) {
        let SearchChoice { open, pin } = hit;
        let (path, jump) = match open {
            SearchOpen::File(path) => (path, None),
            SearchOpen::Match { path, range, text } => (path, Some(Jump::Select { range, text })),
        };
        if !self.tabs.is_empty() && self.tab().doc.path() == Some(path.as_path()) {
            if pin {
                self.tab_mut().preview = false;
            }
            let Some(Jump::Select { range, text }) = jump else {
                return;
            };
            // Field borrow, so `search` and the document are borrowed together.
            let located = self
                .search
                .locate(&self.tabs[self.active].doc, range, &text);
            let Some(range) = located else {
                return;
            };
            let here = self.here();
            self.show_match(Selection {
                anchor: range.start,
                head: range.end,
            });
            self.remember(here);
            return;
        }
        self.open_tab(path.clone(), pin);
        if let Some(jump) = jump {
            self.remember_open(path, Some(jump), None);
        }
    }

    fn apply_browser(&mut self, output: &BrowserOutput) {
        if output.open_folder {
            self.spawn_dialog(DialogKind::Folder);
        }
        if let Some(open) = &output.open_file {
            self.open_tab(open.path.clone(), open.pin);
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
    /// Another file opens in the preview tab.
    #[cfg(test)]
    fn follow(&mut self, at: usize) {
        self.follow_link(at, false);
    }

    /// Follows the link at `at`. `pin` opens another file in a new pinned tab.
    fn follow_link(&mut self, at: usize, pin: bool) {
        let link = {
            let tab = self.tab();
            inkmark_parse::link_at(&tab.doc, tab.parse.output(), at)
        };
        let Some(link) = link else {
            return;
        };
        let here = self.here();
        let dest = match link {
            inkmark_parse::Link::Footnote(label) => {
                self.pin_if(pin);
                let offset = {
                    let tab = self.tab();
                    inkmark_parse::footnote_offset(&tab.doc, tab.parse.output(), &label)
                };
                match offset {
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
        let open_path = self.tab().doc.path().map(Path::to_path_buf);
        let parent = open_path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        let base = parent.unwrap_or_else(|| self.browser.root().to_path_buf());
        match links::resolve(&dest, &base) {
            links::Target::External(url) => self.open_external(url),
            links::Target::Document(path) => {
                self.open_external(path.to_string_lossy().into_owned());
            }
            links::Target::Anchor(anchor) => {
                self.pin_if(pin);
                if self.jump_to_anchor(&anchor) {
                    self.remember(here);
                }
            }
            links::Target::File { path, anchor }
                if Some(path.as_path()) == open_path.as_deref() =>
            {
                self.pin_if(pin);
                if anchor.is_none_or(|a| self.jump_to_anchor(&a)) {
                    self.remember(here);
                }
            }
            links::Target::File { path, .. } if !path.exists() => {
                self.show_hint(format!("{} doesn't exist", display_name(&path)));
            }
            links::Target::File { path, anchor } => {
                let jump = anchor.map(Jump::Anchor);
                self.open_tab(path.clone(), pin);
                if self.open_landed(&path).is_some() {
                    self.remember_open(path, jump, Some((here, Travel::Follow)));
                }
            }
            links::Target::Refused(reason) => self.show_hint(reason),
        }
    }

    /// Where the caret is now, as a place to come back to.
    fn here(&self) -> Place {
        let path = self.tab().doc.path().map(Path::to_path_buf);
        Place {
            path,
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
        let open = self.tab().doc.path().map(Path::to_path_buf);
        match &place.path {
            // Gone since (deleted, or moved by another program): say so,
            // and keep the place, rather than open an empty file there.
            Some(path) if Some(path.as_path()) != open.as_deref() && !path.exists() => {
                self.show_hint(format!("{} no longer exists", display_name(path)));
                self.unwind(travel);
            }
            Some(path) if Some(path.as_path()) != open.as_deref() => {
                let path = path.clone();
                let offset = place.offset;
                self.request_open(path.clone());
                if self.open_landed(&path).is_some() {
                    self.remember_open(path, Some(Jump::Offset(offset)), Some((here, travel)));
                } else {
                    self.unwind(travel);
                }
            }
            _ => {
                self.commit(here, travel);
                let len = self.tab().doc.len();
                self.jump_to(place.offset.min(len));
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

    /// The tab `open_tab` just activated, when its file is exactly `path`.
    fn open_landed(&self, path: &Path) -> Option<u64> {
        let tab = self.tabs.get(self.active)?;
        (tab.doc.path() == Some(path)).then_some(tab.id)
    }

    /// Records a jump and a history change against the tab that opened
    /// `path`. Does nothing when that open did not land there.
    fn remember_open(
        &mut self,
        path: PathBuf,
        jump: Option<Jump>,
        history: Option<(Place, Travel)>,
    ) {
        let Some(tab) = self.open_landed(&path) else {
            return;
        };
        if let Some(jump) = jump {
            self.pending_jump = Some(PendingJump {
                tab,
                path: path.clone(),
                jump,
            });
        }
        if let Some((here, travel)) = history {
            self.pending_history = Some(PendingHistory {
                tab,
                path,
                here,
                travel,
            });
        }
    }

    /// Settles a history change waiting on an open: done once that tab
    /// exists with the file it was opened for, even if another tab is
    /// showing. Undone if that tab is gone or its file changed.
    fn settle_history(&mut self) {
        let Some(pending) = self.pending_history.clone() else {
            return;
        };
        let Some(index) = self.index_of_tab(pending.tab) else {
            if self.confirm.is_none() {
                self.pending_history = None;
                self.unwind(pending.travel);
            }
            return;
        };
        if self.tabs[index].doc.path() != Some(pending.path.as_path()) {
            if self.confirm.is_none() {
                self.pending_history = None;
                self.unwind(pending.travel);
            }
            return;
        }
        self.pending_history = None;
        self.commit(pending.here, pending.travel);
    }

    fn jump_to_anchor(&mut self, anchor: &str) -> bool {
        self.jump_to_anchor_in(self.active, anchor)
    }

    fn jump_to_anchor_in(&mut self, index: usize, anchor: &str) -> bool {
        let offset = {
            let tab = &self.tabs[index];
            inkmark_parse::heading_offset(&tab.doc, tab.parse.output(), anchor)
        };
        match offset {
            Some(offset) => {
                self.jump_to_in(index, offset);
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
        self.jump_to_in(self.active, offset);
    }

    fn jump_to_in(&mut self, index: usize, offset: usize) {
        let caret = inkmark_buffer::Selection::caret(offset);
        let tab = &mut self.tabs[index];
        tab.code.set_selection(caret);
        tab.live.set_selection(caret);
    }

    /// Applies a jump waiting on the tab an open landed in, once that tab's
    /// parse has settled. Another tab being on screen does not drop it.
    /// Dropped if that tab is gone or its file changed.
    fn apply_pending_jump(&mut self) {
        let Some(pending) = self.pending_jump.clone() else {
            return;
        };
        let Some(index) = self.index_of_tab(pending.tab) else {
            if self.confirm.is_none() {
                self.pending_jump = None;
            }
            return;
        };
        if self.tabs[index].doc.path() != Some(pending.path.as_path()) {
            if self.confirm.is_none() {
                self.pending_jump = None;
            }
            return;
        }
        // A background tab is not drawn, so this is what accepts its parse.
        let wait = {
            let tab = &mut self.tabs[index];
            tab.parse.update(&tab.doc)
        };
        if let Some(wait) = wait {
            self.ctx.request_repaint_after(wait);
        }
        if !self.tabs[index].parse.is_settled() {
            return;
        }
        self.pending_jump = None;
        match pending.jump {
            Jump::Anchor(anchor) => {
                self.jump_to_anchor_in(index, &anchor);
            }
            Jump::Offset(offset) => {
                let len = self.tabs[index].doc.len();
                self.jump_to_in(index, offset.min(len));
            }
            Jump::Select { range, text } => {
                // Field borrow, so `search` and the document are borrowed together.
                let located = self.search.locate(&self.tabs[index].doc, range, &text);
                if let Some(range) = located {
                    self.show_match_in(
                        index,
                        Selection {
                            anchor: range.start,
                            head: range.end,
                        },
                    );
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

    /// The tab on screen parses. So does a tab whose jump is still waiting
    /// on its parse. Every other worker sleeps, and after a pause its
    /// in-flight job is dropped.
    fn sync_parsers(&mut self) {
        let pending = self.pending_jump.as_ref().map(|jump| jump.tab);
        let active = self.active;
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let awake = index == active || Some(tab.id) == pending;
            tab.parse.set_awake(awake);
            if !awake {
                tab.parse.park_if_idle();
            }
        }
    }

    fn share_images(&mut self, ctx: &egui::Context) {
        if self.images.is_none() {
            self.images = Some(inkmark_view::ImageCache::new(ctx));
        }
        let Some(cache) = self.images.clone() else {
            return;
        };
        if let Some(id) = self.tabs.get(self.active).map(|tab| tab.id) {
            cache.set_active(id);
        }
        for tab in &mut self.tabs {
            let id = tab.id;
            tab.live.share_images(cache.clone(), id);
        }
    }

    /// The tab closed or its file was replaced. Its images are dropped
    /// unless another open tab is showing them.
    fn forget_tab_images(&self, id: u64) {
        if let Some(cache) = &self.images {
            cache.release_user(id);
        }
    }

    /// Estimated shaped-line bytes kept across every tab (both copies of
    /// the source, plus the glyphs). Past this, tabs that are not showing
    /// drop their line caches first. The tab on screen keeps its own, so
    /// switching back to it is still a paint.
    const LINE_CACHE_BUDGET: usize = 8 * 1024 * 1024;

    fn trim_background_line_caches(&mut self) {
        self.trim_background_line_caches_over(Self::LINE_CACHE_BUDGET);
    }

    fn trim_background_line_caches_over(&mut self, budget: usize) {
        let total: usize = self
            .tabs
            .iter()
            .map(|tab| tab.code.cached_line_bytes() + tab.live.cached_line_bytes())
            .sum();
        if total <= budget {
            return;
        }
        let active = self.active;
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if index == active {
                continue;
            }
            tab.code.release_line_cache();
            tab.live.release_line_cache();
        }
    }

    fn panes(&mut self, ui: &mut egui::Ui) {
        self.sync_parsers();
        self.share_images(ui.ctx());
        let ctx = ui.ctx().clone();
        let hint = self.tab_mut().live.take_hint();
        if let Some(hint) = hint {
            self.show_hint(hint);
        }
        // Ctrl+clicks from the last frame, now that the click is over.
        let followed = {
            let tab = self.tab_mut();
            tab.live.take_follow().or(tab.code.take_follow())
        };
        if let Some(click) = followed {
            self.follow_link(click.at, click.pin);
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
                let tab = self.tab_mut();
                tab.code.show(ui, &mut tab.doc, Some(&mut tab.parse));
            }
            Mode::Live => {
                let tab = self.tab_mut();
                tab.live.show(ui, &mut tab.doc, Some(&mut tab.parse));
            }
            Mode::Split => {
                let rect = ui.available_rect_before_wrap();
                let fraction = self.layout.split_fraction(rect.width());
                let mid = layout::split_mid(rect.left(), rect.width(), fraction);
                let half = layout::SPLIT_GAP / 2.0;
                let left = Rect::from_min_max(rect.min, pos2(mid - half, rect.bottom()));
                let right = Rect::from_min_max(pos2(mid + half, rect.top()), rect.max);
                ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
                    let tab = self.tab_mut();
                    tab.code.show(ui, &mut tab.doc, Some(&mut tab.parse));
                });
                ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
                    let tab = self.tab_mut();
                    tab.live.show(ui, &mut tab.doc, Some(&mut tab.parse));
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
        let mode = self.mode;
        {
            let tab = self.tab_mut();
            if tab.code.has_focus(&ctx) {
                tab.focus = Pane::Code;
            } else if tab.live.has_focus(&ctx) {
                tab.focus = Pane::Live;
            }
            // The other pane mirrors the caret and follows the scroll position.
            let (code_scrolled, live_scrolled) =
                (tab.code.take_scrolled(), tab.live.take_scrolled());
            let parse = tab.parse.output();
            match tab.focus {
                Pane::Code => tab.live.mirror_selection(tab.code.selection()),
                Pane::Live => tab.code.mirror_selection(tab.live.selection()),
            }
            if mode == Mode::Split && parse.map.len() == tab.doc.len() {
                if code_scrolled {
                    let pos = tab.code.scroll_pos();
                    tab.live.set_scroll_pos(&tab.doc, parse, pos);
                    ctx.request_repaint();
                } else if live_scrolled {
                    let pos = tab.live.scroll_pos(&tab.doc, parse);
                    tab.code.set_scroll_pos(pos);
                    ctx.request_repaint();
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(m) = &mut self.measure {
            m.input(raw_input);
        }
    }
}

impl App {
    /// One frame of the whole app. Separate from `eframe::App::ui` so tests
    /// drive exactly this, without a real window.
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if std::mem::take(&mut self.refocus_pane) && !self.tabs.is_empty() {
            let pane = self.tab().focus;
            self.focus_pane(&ctx, pane);
        }
        self.os_theme.poll(&ctx);
        self.note_session(&ctx);
        self.handle_shortcuts(&ctx);
        self.poll_dialog(&ctx);
        self.check_disk(&ctx);
        self.guard_close(&ctx);
        // Dialogs take the keyboard before the panes see it.
        self.recent_ui(&ctx);
        self.keys_ui(&ctx);
        self.syntax_ui(&ctx);
        self.hold_focus_for_dialogs(&ctx);

        let show_banner = self.error.is_some()
            || self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.banner.is_some());
        if show_banner {
            egui::Panel::top("banner").show(ui, |ui| self.banner_ui(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_ui(ui));
        if self.find_open && !self.tabs.is_empty() {
            self.find_history_keys(&ctx);
            let selection = self.selection();
            let modal = self.modal_open();
            let step = {
                let tab = self.tab_mut();
                egui::Panel::bottom("find")
                    .show(ui, |ui| tab.find.show(ui, &mut tab.doc, selection, modal))
                    .inner
            };
            if step.closed {
                self.find_open = false;
                let focus = self.tab().focus;
                self.focus_pane(&ctx, focus);
            }
            if let Some(sel) = step.selection {
                self.show_match(sel);
            }
        }
        if self.goto.is_open() && !self.tabs.is_empty() {
            let line_count = self.tab().doc.line_count();
            let modal = self.modal_open();
            let step = {
                let goto = &mut self.goto;
                egui::Panel::bottom("goto")
                    .show(ui, |ui| goto.show(ui, line_count, modal))
                    .inner
            };
            if let Some(line) = step.line {
                let here = self.here();
                let offset = self.tab().doc.line_to_byte(line);
                self.jump_to(offset);
                self.remember(here);
                let focus = self.tab().focus;
                self.focus_pane(&ctx, focus);
            } else if step.closed {
                let focus = self.tab().focus;
                self.focus_pane(&ctx, focus);
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
        self.trim_background_line_caches();
        self.pin_edited_preview();
        self.finish_keys_focus(&ctx);
    }
}

impl App {
    fn measure_step(&mut self, ctx: &egui::Context) {
        if self.tabs.is_empty() {
            return;
        }
        let settled = self.tab().parse.is_settled();
        let lines = self.tab().doc.line_count();
        let Some(m) = self.measure.as_mut() else {
            return;
        };
        let step = m.frame(ctx, settled, lines);
        match step {
            measure::Step::Idle => {}
            measure::Step::ScrollTo(line) => {
                let pos = inkmark_view::ScrollPos { line, frac: 0.0 };
                let tab = self.tab_mut();
                tab.code.set_scroll_pos(pos);
                tab.live.set_scroll_pos(&tab.doc, tab.parse.output(), pos);
            }
            measure::Step::Caret(pane) => {
                let pane = match pane {
                    measure::Pane::Code => Pane::Code,
                    measure::Pane::Live => Pane::Live,
                };
                // The middle of a line in the middle of the document.
                let at = {
                    let doc = &self.tab().doc;
                    let line = doc.line_count() / 2;
                    let range = doc.line_range(line);
                    let mut at = (range.start + range.end) / 2;
                    while !doc.is_char_boundary(at) {
                        at += 1;
                    }
                    at
                };
                self.tab_mut().focus = pane;
                self.jump_to(at);
                self.focus_pane(ctx, pane);
            }
            measure::Step::Save => {
                let started = std::time::Instant::now();
                self.save();
                if let Some(m) = &mut self.measure {
                    m.saved(started.elapsed());
                }
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

/// Two paths that name one file. A missing file is the nearest existing
/// ancestor's canonical path plus the unresolved suffix, so `sub/../draft.md`
/// and `draft.md` are the same file before either exists.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    normalized_path(a) == normalized_path(b)
}

/// `path` with `.` and `..` collapsed, then the longest existing prefix
/// canonicalized. The missing tail is appended unchanged.
fn normalized_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let collapsed = collapse_dots(&absolute);
    let mut missing = Vec::new();
    let mut cursor = collapsed.as_path();
    loop {
        if cursor.as_os_str().is_empty() {
            return collapsed;
        }
        if let Ok(canon) = std::fs::canonicalize(cursor) {
            let mut out = canon;
            for part in missing.iter().rev() {
                out.push(part);
            }
            return out;
        }
        let Some(parent) = cursor.parent() else {
            return collapsed;
        };
        if parent == cursor {
            return collapsed;
        }
        if let Some(name) = cursor.file_name() {
            missing.push(name.to_os_string());
        }
        cursor = parent;
    }
}

/// Lexical `.` and `..`, without asking the filesystem. An absolute path
/// cannot climb above its root.
fn collapse_dots(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
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
        tab_text(app, app.active)
    }

    fn tab_text(app: &App, index: usize) -> String {
        let doc = &app.tabs[index].doc;
        doc.slice(0..doc.len()).into_owned()
    }

    fn type_into(app: &mut App, s: &str) {
        let doc = &mut app.tab_mut().doc;
        let end = doc.len();
        doc.apply(
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
    fn the_window_holds_one_tab() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path(), None);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.active, 0);
        assert!(!app.tab().preview);
        assert!(!app.find_open);
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
        assert!(app.tab().doc.is_dirty());
        assert!(matches!(app.tab().banner, Some(Banner::DiskMissing)));
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
        assert!(matches!(app.tab().banner, Some(Banner::DiskChanged)));
        // Saving somewhere else is fine.
        app.save_as(dir.path().join("copy.md"));
        assert_eq!(
            fs::read_to_string(dir.path().join("copy.md")).unwrap(),
            "one\nmine\n"
        );
    }

    #[test]
    fn a_file_chosen_in_the_open_dialog_keeps_the_unsaved_tab() {
        // Regression for #7: the dialog used to drop the draft. The draft
        // stays, and the chosen file opens beside it.
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
        assert!(app.confirm.is_none());
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(tab_text(&app, 0), "draft");
        assert_eq!(app.tab().doc.path(), Some(other.as_path()));
        assert_eq!(text(&app), "other\n");
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
        assert_eq!(opened.tab().doc.path(), Some(file.as_path()));

        let browsed = app(dir.path(), Some(folder.clone()));
        assert_eq!(browsed.browser.root(), folder);
        assert!(browsed.tab().doc.path().is_none());
        assert!(browsed.recent_list.is_none());
        // Browsing a folder does not remember the folder as a file. The
        // shared test store may still hold the file opened above.
        assert!(browsed.recent.entries().iter().all(|p| p != &folder));

        let created = app(dir.path(), Some(missing.clone()));
        assert_eq!(created.browser.root(), folder);
        assert_eq!(created.tab().doc.path(), Some(missing.as_path()));
        assert_eq!(text(&created), "");

        let cwd = std::env::current_dir().unwrap();
        let ctx = egui::Context::default();
        // A state directory of its own: the opens above wrote a session
        // next to `recent`, and a no-argument launch would restore it.
        let fresh = dir.path().join("fresh-state");
        fs::create_dir(&fresh).unwrap();
        let here = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(fresh.join("recent"))),
        );
        assert_eq!(here.browser.root(), cwd);
        assert!(here.tab().doc.path().is_none());
        assert!(here.recent_list.is_none());

        // Recent files are still offered when nothing was passed, and not
        // when a folder was.
        let remembered = dir.path().join("old.md");
        fs::write(&remembered, "old\n").unwrap();
        let state = dir.path().join("recent-state");
        fs::create_dir(&state).unwrap();
        let store = state.join("recent");
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
        app.tab_mut().focus = Pane::Live;
        let pos = inkmark_view::ScrollPos {
            line: 120,
            frac: 0.0,
        };
        let parse = app.tab().parse.output().clone();
        {
            let tab = app.tab_mut();
            tab.live.set_scroll_pos(&tab.doc, &parse, pos);
        }
        app.cycle_mode(&ctx);
        assert!(app.mode == Mode::Split);
        assert_eq!(app.tab().code.scroll_pos(), pos);
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
        assert!(app.tab().code.show_minimap && app.tab().live.show_minimap);

        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::E, egui::Modifiers::COMMAND)],
        );
        assert!(app.mode == Mode::Code && app.tab().focus == Pane::Code);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::M, egui::Modifiers::COMMAND)],
        );
        assert!(!app.tab().code.show_minimap);
        assert!(app.tab().live.show_minimap);
        let stored = fs::read_to_string(&layout_path).unwrap();
        assert!(stored.starts_with("code\n0\n1\n"), "{stored}");

        let ctx = egui::Context::default();
        let mut app = App::with_recent(&ctx, None, recent::Recent::from_store(Some(store.clone())));
        assert!(app.mode == Mode::Code && app.tab().focus == Pane::Code);
        assert!(!app.tab().code.show_minimap);
        assert!(app.tab().live.show_minimap);

        // Ctrl+2 leaves code-only and shows the live pane.
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Num2, egui::Modifiers::COMMAND)],
        );
        assert!(app.mode == Mode::Live && app.tab().focus == Pane::Live);
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::M, egui::Modifiers::COMMAND)],
        );
        assert!(!app.tab().code.show_minimap && !app.tab().live.show_minimap);

        let ctx = egui::Context::default();
        let app = App::with_recent(&ctx, None, recent::Recent::from_store(Some(store)));
        assert!(app.mode == Mode::Live && app.tab().focus == Pane::Live);
        assert!(!app.tab().code.show_minimap && !app.tab().live.show_minimap);
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
        assert!(reloaded.tab().code.show_minimap && reloaded.tab().live.show_minimap);
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
        assert_eq!(app.tab().doc.path(), Some(file.as_path()));
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
        assert_eq!(app.tab().doc.path(), Some(file.as_path()));
        assert_eq!(text(&app), "keep\n");
    }

    #[test]
    fn opening_from_the_sidebar_keeps_unsaved_edits_and_focuses_an_open_file() {
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
            Some(a.clone()),
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        type_into(&mut app, "unsaved");
        let mut time = 0.0;
        wait_rows(&ctx, &mut app, &mut time);
        let rect = app.browser.row_rect(&b).expect("b.md should be on screen");
        click_at(&ctx, &mut app, &mut time, rect.center());
        assert!(app.confirm.is_none());
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tab().doc.path(), Some(b.as_path()));
        assert_eq!(text(&app), "b\n");
        assert_eq!(tab_text(&app, 0), "a\nunsaved");

        // A second click on a.md focuses the tab that already has it.
        let again = app.browser.row_rect(&a).expect("a.md should be on screen");
        click_at(&ctx, &mut app, &mut time, again.center());
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tab().doc.path(), Some(a.as_path()));
        assert_eq!(text(&app), "a\nunsaved");
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
        assert_eq!(app.tab().doc.path(), Some(created.as_path()));
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
        assert_eq!(app.tab().doc.path(), Some(created.as_path()));

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
        assert_eq!(app.tab().doc.path(), Some(also.as_path()));

        // Unsaved edits stay in their tab. The new file opens beside them.
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
        assert!(app.confirm.is_none());
        assert_eq!(app.tab().doc.path(), Some(fresh.as_path()));
        assert_eq!(text(&app), "");
        let kept = app
            .tabs
            .iter()
            .find(|tab| tab.doc.path() == Some(also.as_path()))
            .expect("the dirty tab stays open");
        assert_eq!(kept.doc.slice(0..kept.doc.len()).as_ref(), "dirty");

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
        assert_eq!(app.tab().doc.path(), Some(plain.as_path()));
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
        assert!(app.tab().banner.is_none());

        fs::write(&open, "changed by someone else\n").unwrap();
        app.next_disk_check = std::time::Instant::now();
        app.check_disk(&ctx);
        assert!(matches!(app.tab().banner, Some(Banner::DiskChanged)));
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
        assert!(matches!(app.tab().banner, Some(Banner::DiskChanged)));
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
        assert_eq!(app.tab().doc.path(), Some(new.as_path()));
        assert_eq!(text(&app), "one\nunsaved\n");
        assert!(app.tab().doc.is_dirty());
        // The rename itself isn't a change on disk.
        app.next_disk_check = Instant::now();
        app.check_disk(&ctx);
        assert!(app.tab().banner.is_none());
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
        assert_eq!(app.tab().doc.path(), Some(a.as_path()));
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
        assert_eq!(app.tab().doc.path(), Some(moved.as_path()));
        app.next_disk_check = Instant::now();
        app.check_disk(&ctx);
        assert!(app.tab().banner.is_none());

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
        assert_eq!(app.tab().doc.path(), Some(moved.as_path()));
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
        assert!(matches!(app.tab().banner, Some(Banner::DiskMissing)));
    }

    #[test]
    fn renaming_a_background_tab_keeps_its_edits_on_the_new_path() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("a.md");
        let other = dir.path().join("b.md");
        fs::write(&old, "a\n").unwrap();
        fs::write(&other, "b\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(old.clone()));
        type_into(&mut app, "unsaved\n");
        app.request_open(other.clone());
        assert_eq!(app.tab().doc.path(), Some(other.as_path()));
        app.begin_rename(old.clone());
        app.rename.as_mut().unwrap().name = "renamed.md".into();
        let mut time = 0.0;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let new = dir.path().join("renamed.md");
        assert!(app.rename.is_none());
        assert_eq!(app.tabs[0].doc.path(), Some(new.as_path()));
        assert_eq!(tab_text(&app, 0), "a\nunsaved\n");
        assert!(app.tabs[0].doc.is_dirty());
        assert_eq!(app.tab().doc.path(), Some(other.as_path()));
        app.activate(0);
        app.next_disk_check = Instant::now();
        app.check_disk(&ctx);
        assert!(
            app.tab().banner.is_none(),
            "the rename is not a disk change"
        );
        app.save();
        assert_eq!(fs::read_to_string(&new).unwrap(), "a\nunsaved\n");
        assert!(!old.exists(), "saving must not recreate the old path");
    }

    #[test]
    fn moving_a_background_files_folder_follows_that_tab() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        let archive = dir.path().join("archive");
        fs::create_dir(&notes).unwrap();
        fs::create_dir(&archive).unwrap();
        let file = notes.join("a.md");
        let other = dir.path().join("b.md");
        fs::write(&file, "a\n").unwrap();
        fs::write(&other, "b\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(file.clone()));
        type_into(&mut app, "unsaved\n");
        app.request_open(other.clone());
        let (tx, rx) = mpsc::channel();
        app.dialog = Some(rx);
        tx.send(DialogResult::MoveTo(notes.clone(), Some(archive.clone())))
            .unwrap();
        app.poll_dialog(&ctx);
        let moved = archive.join("notes/a.md");
        assert!(moved.exists());
        assert!(!file.exists());
        assert_eq!(app.tabs[0].doc.path(), Some(moved.as_path()));
        assert_eq!(tab_text(&app, 0), "a\nunsaved\n");
        assert!(app.tabs[0].doc.is_dirty());
        assert_eq!(app.tab().doc.path(), Some(other.as_path()));
        app.activate(0);
        app.save();
        assert_eq!(fs::read_to_string(&moved).unwrap(), "a\nunsaved\n");
        assert!(!file.exists());
    }

    #[test]
    fn trashing_a_background_file_marks_that_tab_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.md");
        let other = dir.path().join("b.md");
        fs::write(&file, "a\n").unwrap();
        fs::write(&other, "b\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(file.clone()));
        type_into(&mut app, "unsaved\n");
        app.request_open(other.clone());
        app.trash = Box::new(FolderTrash(bin.path().to_path_buf()));
        app.trash_confirm = Some(file.clone());
        let mut time = 0.0;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert!(!file.exists());
        assert!(bin.path().join("a.md").exists());
        assert_eq!(tab_text(&app, 0), "a\nunsaved\n");
        assert!(matches!(app.tabs[0].banner, Some(Banner::DiskMissing)));
        assert!(app.tabs[1].banner.is_none());
        assert_eq!(app.tab().doc.path(), Some(other.as_path()));
        assert_eq!(text(&app), "b\n");
    }

    /// Frames until the parse is in and any jump from a followed link is done.
    fn settle(ctx: &egui::Context, app: &mut App, time: &mut f64) {
        let start = Instant::now();
        while !app.tab().parse.is_settled() || app.pending_jump.is_some() {
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
        assert_eq!(app.tab().doc.path(), Some(b.as_path()));
        assert_eq!(app.selection().head, offset_of(&app, "Second part"));
        // Alt+Left: back to a.md, where the caret was.
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::ArrowLeft, egui::Modifiers::ALT)],
        );
        settle(&ctx, &mut app, &mut time);
        assert_eq!(app.tab().doc.path(), Some(a.as_path()));
        assert_eq!(app.selection().head, clicked_from);
        assert!(app.back.is_empty());
    }

    #[test]
    fn following_a_link_keeps_unsaved_edits() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "[next](b.md)\n").unwrap();
        fs::write(&b, "# B\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        type_into(&mut app, "draft");
        drive(&ctx, &mut app, &mut time, vec![]);
        app.follow(1);
        assert!(app.confirm.is_none());
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tab().doc.path(), Some(b.as_path()));
        assert!(app.tab().preview);
        assert!(tab_text(&app, 0).ends_with("draft"));
    }

    #[test]
    fn a_middle_click_on_a_link_opens_a_pinned_tab() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "[next](b.md)\n").unwrap();
        fs::write(&b, "# B\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.sidebar.visible = false;
        run.frame(vec![egui::Event::ModifiersChanged(
            egui::Modifiers::COMMAND,
        )]);
        let mut found = None;
        for x in (20..240).step_by(4) {
            for y in [16.0_f32, 24.0, 32.0] {
                let pos = egui::pos2(x as f32, y);
                run.frame(vec![egui::Event::PointerMoved(pos)]);
                if run.out.platform_output.cursor_icon == egui::CursorIcon::PointingHand {
                    found = Some(pos);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let pos = found.expect("the link shows a hand under Ctrl");
        run.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
        run.click_pos(pos, egui::PointerButton::Middle);
        run.settle();
        assert_eq!(run.app.tabs.len(), 2);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(!run.app.tab().preview);
        assert_eq!(run.app.tabs[0].doc.path(), Some(a.as_path()));
        assert_eq!(tab_text(&run.app, 0), "[next](b.md)\n");
    }

    #[test]
    fn a_middle_click_on_a_same_file_link_pins_the_preview() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "[see](#part)\n\n## Part\n\nend\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.sidebar.visible = true;
        let at = run.row_center(&b);
        run.click_pos(at, egui::PointerButton::Primary);
        run.settle();
        assert_eq!(run.app.tabs.len(), 2);
        assert!(run.app.tab().preview);
        run.app.sidebar.visible = false;
        run.frame(vec![]);
        let pos = run.link_hand();
        run.click_pos(pos, egui::PointerButton::Middle);
        run.settle();
        assert_eq!(run.app.tabs.len(), 2);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(!run.app.tab().preview);
        assert_eq!(run.app.selection().head, offset_of(&run.app, "Part"));
        assert!(
            run.app
                .tabs
                .iter()
                .filter(|tab| tab.doc.path() == Some(b.as_path()))
                .count()
                == 1
        );
    }

    #[test]
    fn a_middle_click_on_a_footnote_pins_the_preview() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "See [^1].\n\n[^1]: the note\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.sidebar.visible = true;
        let at = run.row_center(&b);
        run.click_pos(at, egui::PointerButton::Primary);
        run.settle();
        assert!(run.app.tab().preview);
        run.app.sidebar.visible = false;
        run.frame(vec![]);
        let pos = run.link_hand();
        run.click_pos(pos, egui::PointerButton::Middle);
        run.settle();
        assert_eq!(run.app.tabs.len(), 2);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(!run.app.tab().preview);
        assert_eq!(run.app.selection().head, offset_of(&run.app, "the note"));
    }

    #[test]
    fn a_middle_click_on_another_file_leaves_the_source_preview() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "[next](b.md)\n").unwrap();
        fs::write(&b, "# B\n").unwrap();
        fs::write(&c, "gamma\n").unwrap();
        let mut run = Run::new(dir.path(), Some(c));
        run.app.sidebar.visible = true;
        let at = run.row_center(&a);
        run.click_pos(at, egui::PointerButton::Primary);
        run.settle();
        assert!(run.app.tab().preview);
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.app.sidebar.visible = false;
        run.frame(vec![]);
        let pos = run.link_hand();
        run.click_pos(pos, egui::PointerButton::Middle);
        run.settle();
        assert_eq!(run.app.tabs.len(), 3);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(!run.app.tab().preview);
        assert!(
            run.app
                .tabs
                .iter()
                .any(|tab| tab.doc.path() == Some(a.as_path()) && tab.preview)
        );
    }

    #[test]
    fn a_followed_link_keeps_its_jump_after_switching_away() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "Intro.\n\nSee [the second part](b.md#second-part).\n").unwrap();
        fs::write(&b, "# First\n\ntext\n\n## Second part\n\nend\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        let from = offset_of(&run.app, "Intro") + 2;
        run.app.jump_to(from);
        run.app.follow(offset_of(&run.app, "second part"));
        assert!(run.app.pending_jump.is_some());
        assert!(run.app.pending_history.is_some());
        run.app.activate(0);
        let kept = run.app.tab().code.selection();
        run.settle();
        assert!(run.app.pending_history.is_none());
        assert!(!run.app.back.is_empty());
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        assert_eq!(run.app.tab().code.selection(), kept);
        let opened = run
            .app
            .tabs
            .iter()
            .find(|tab| tab.doc.path() == Some(b.as_path()))
            .expect("b stayed open");
        let head = opened.code.selection().head;
        let end = opened.doc.len().min(head + "Second part".len());
        assert_eq!(opened.doc.slice(head..end).as_ref(), "Second part");
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
        assert_eq!(app.tab().doc.path(), Some(a.as_path()));
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
        assert_eq!(app.tab().doc.path(), Some(path.as_path()));
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
        assert_eq!(app.tab().doc.path(), Some(renamed.as_path()));
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
        assert_eq!(app.tab().doc.path(), Some(b.as_path()));
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::ArrowLeft, egui::Modifiers::ALT)],
        );
        assert_eq!(app.tab().doc.path(), Some(b.as_path()));
        assert_eq!(app.selection().head, 2);
        assert_eq!(text(&app), "Claim[^1].\n\n[^1]: Source.\n");

        // A place in a file deleted since: say so, stay put.
        let gone = dir.path().join("gone.md");
        app.back.push(Place {
            path: Some(gone.clone()),
            offset: 0,
        });
        app.go_back();
        assert_eq!(app.tab().doc.path(), Some(b.as_path()));
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
        assert_eq!(app.tab().doc.path(), Some(a.as_path()));
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
        assert!(app.mode == Mode::Split && app.tab().focus == Pane::Code);
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
        assert_eq!(app.tab().live.selection().head, note);
        assert_eq!(app.tab().code.selection().head, note);
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
            self.click_text_button(text, egui::PointerButton::Primary);
        }

        /// Clicks a tab chip. Marks share the title's galley, so the title
        /// alone is not on screen once a chip is dirty, changed, or missing.
        fn click_tab(&mut self, title: &str) {
            let candidates = [
                title.to_owned(),
                format!("{title} ●"),
                format!("{title} ↻"),
                format!("{title} ✕"),
                format!("{title} ● ↻"),
                format!("{title} ● ✕"),
            ];
            for _ in 0..3 {
                if candidates.iter().any(|text| self.text_rect(text).is_some()) {
                    break;
                }
                self.frame(vec![]);
            }
            let shown = candidates
                .iter()
                .find(|text| self.text_rect(text).is_some())
                .unwrap_or_else(|| panic!("{title:?} isn't on screen"));
            self.click_text(shown);
        }

        fn click_text_button(&mut self, text: &str, button: egui::PointerButton) {
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
            let press = |pressed| egui::Event::PointerButton {
                pos: at,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(at)]);
            self.frame(vec![press(true)]);
            self.frame(vec![press(false)]);
            self.frame(vec![]);
        }

        /// Where a link under the pointer shows a hand while Ctrl is held.
        fn link_hand(&mut self) -> egui::Pos2 {
            self.frame(vec![egui::Event::ModifiersChanged(
                egui::Modifiers::COMMAND,
            )]);
            let mut found = None;
            for x in (8..480).step_by(4) {
                for y in (12..96).step_by(4) {
                    let pos = egui::pos2(x as f32, y as f32);
                    self.frame(vec![egui::Event::PointerMoved(pos)]);
                    if self.out.platform_output.cursor_icon == egui::CursorIcon::PointingHand {
                        found = Some(pos);
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            self.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
            found.expect("the link shows a hand under Ctrl")
        }

        fn click_pos(&mut self, pos: egui::Pos2, button: egui::PointerButton) {
            let press = |pressed| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(pos), press(true)]);
            self.frame(vec![press(false)]);
            self.frame(vec![]);
        }

        fn row_center(&mut self, path: &Path) -> egui::Pos2 {
            let start = Instant::now();
            loop {
                if let Some(row) = self.app.browser.row_rect(path) {
                    return row.center();
                }
                self.frame(vec![]);
                assert!(
                    start.elapsed() < Duration::from_secs(2),
                    "sidebar listing did not arrive"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
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
            while !self.app.tab().parse.is_settled() || self.app.pending_jump.is_some() {
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

        fn wait_find(&mut self) {
            let start = Instant::now();
            while self.app.tab().find.scanning() {
                self.frame(vec![]);
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "regex find did not finish"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            self.frame(vec![]);
        }

        fn check_disk_now(&mut self) {
            self.app.next_disk_check = Instant::now();
            self.frame(vec![]);
        }

        fn frame_sized(&mut self, events: Vec<egui::Event>, size: egui::Vec2) {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            self.run(input);
        }

        /// `text` was drawn inside the clip that contains it, so a row
        /// scrolled off the list does not count.
        fn text_in_view(&self, text: &str) -> bool {
            fn find(shape: &egui::Shape, text: &str) -> Option<egui::Rect> {
                match shape {
                    egui::Shape::Text(t) if t.galley.text() == text => {
                        Some(t.galley.rect.translate(t.pos.to_vec2()))
                    }
                    egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, text)),
                    _ => None,
                }
            }
            self.out.shapes.iter().any(|clip| {
                find(&clip.shape, text).is_some_and(|rect| clip.clip_rect.intersects(rect))
            })
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
        assert_eq!(run.app.tab().code.selection().range(), 4..7);
        assert_eq!(run.app.tab().find.match_count(), 2);
        assert_eq!(run.app.tab().find.match_index(), Some(1));

        run.app.tab_mut().find.set_replacement("dog");
        run.click_text("Replace all");
        assert_eq!(text(&run.app), "one dog\ntwo dog\n");
        run.key(egui::Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(text(&run.app), "one cat\ntwo cat\n");
        assert!(!run.app.tab().doc.can_undo());
    }

    #[test]
    fn find_query_stays_on_its_tab() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "aaaa aaaa cat\n").unwrap();
        fs::write(&b, "cat dog\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.key(egui::Key::F, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("cat".into())]);
        run.frame(vec![]);
        assert_eq!(run.app.tab().code.selection().range(), 10..13);
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        run.app.open(b);
        run.settle();
        assert_eq!(run.app.tabs.len(), 2);
        // The new tab did not inherit the query or the old match ranges.
        run.key(egui::Key::F3, egui::Modifiers::NONE);
        assert_eq!(text(&run.app), "cat dog\n");
        assert_eq!(run.app.tab().code.selection().range(), 0..0);
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.key(egui::Key::F3, egui::Modifiers::NONE);
        assert_eq!(text(&run.app), "aaaa aaaa cat\n");
        assert_eq!(run.app.tab().code.selection().range(), 10..13);
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
                .tab()
                .doc
                .path()
                .and_then(|path| path.file_name().map(|n| n.to_owned())),
            Some(std::ffi::OsString::from("alpha.md"))
        );
        assert_eq!(text(&run.app), "hello\n");
        assert!(run.app.tab().preview);
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
                .tab()
                .doc
                .path()
                .and_then(|path| path.file_name().map(|n| n.to_owned())),
            Some(std::ffi::OsString::from("beta.md"))
        );
        assert_eq!(
            run.app
                .tab()
                .doc
                .slice(run.app.tab().code.selection().range())
                .as_ref(),
            "token"
        );
        assert!(run.app.tab().preview);
    }

    #[test]
    fn a_single_click_reuses_the_preview_and_a_double_click_pins() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        fs::write(&c, "gamma\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.sidebar.visible = true;
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        assert_eq!(run.app.tabs.len(), 2);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(run.app.tab().preview);
        assert!(run.app.tab().code.has_focus(&run.ctx));
        assert_eq!(text(&run.app), "beta\n");
        assert_eq!(tab_text(&run.app, 0), "alpha\n");

        let at_c = run.row_center(&c);
        run.click_pos(at_c, egui::PointerButton::Primary);
        assert_eq!(run.app.tabs.len(), 2);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
        assert!(run.app.tab().preview);
        assert!(
            run.app
                .tabs
                .iter()
                .all(|tab| tab.doc.path() != Some(b.as_path()))
        );

        // The second click of a double click pins, and the pinned file stays
        // when the next single click opens something else.
        run.click_pos(at_b, egui::PointerButton::Primary);
        run.click_pos(at_b, egui::PointerButton::Primary);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(!run.app.tab().preview);
        assert!(run.app.confirm.is_none());
        run.click_pos(at_c, egui::PointerButton::Primary);
        assert_eq!(run.app.tabs.len(), 3);
        assert_eq!(run.app.tabs[1].doc.path(), Some(b.as_path()));
        assert!(!run.app.tabs[1].preview);
        assert!(run.app.tab().preview);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
    }

    #[test]
    fn editing_a_preview_pins_it() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        fs::write(&c, "gamma\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.sidebar.visible = true;
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        assert!(run.app.tab().preview);
        run.frame(vec![egui::Event::Text("Z".into())]);
        assert!(!run.app.tab().preview);
        assert_eq!(text(&run.app), "Zbeta\n");
        let at_c = run.row_center(&c);
        run.click_pos(at_c, egui::PointerButton::Primary);
        assert_eq!(run.app.tabs.len(), 3);
        assert_eq!(run.app.tabs[1].doc.path(), Some(b.as_path()));
        assert!(!run.app.tabs[1].preview);
        assert_eq!(tab_text(&run.app, 1), "Zbeta\n");
    }

    #[test]
    fn a_double_click_does_not_replace_a_dirty_tab() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.sidebar.visible = true;
        run.frame(vec![egui::Event::Text("Z".into())]);
        let at = run.row_center(&b);
        run.click_pos(at, egui::PointerButton::Primary);
        run.click_pos(at, egui::PointerButton::Primary);
        assert!(run.app.confirm.is_none());
        assert_eq!(run.app.tabs.len(), 2);
        assert_eq!(tab_text(&run.app, 0), "Zalpha\n");
        assert!(!run.app.tabs[0].preview);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(!run.app.tab().preview);
    }

    #[test]
    fn a_missing_preview_is_kept_when_another_file_opens() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        fs::write(&c, "gamma\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.sidebar.visible = true;
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        assert!(run.app.tab().preview);
        fs::remove_file(&b).unwrap();
        run.check_disk_now();
        assert!(matches!(run.app.tab().disk, Some(Banner::DiskMissing)));
        // The listing drops b.md and puts its missing row at the end, on the
        // rectangle c.md just left. A click there is a second click on b.md.
        run.app.open_tab(c.clone(), false);
        assert!(run.app.confirm.is_none());
        assert_eq!(run.app.tabs.len(), 3);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
        assert!(run.app.tab().preview);
        let kept = run
            .app
            .tabs
            .iter()
            .find(|tab| tab.doc.path() == Some(b.as_path()))
            .expect("the missing preview stayed open");
        assert!(!kept.preview);
        assert!(matches!(kept.disk, Some(Banner::DiskMissing)));
        assert_eq!(tab_text(&run.app, 1), "beta\n");
    }

    #[test]
    fn ctrl_tab_onto_a_preview_does_not_pin_it() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.sidebar.visible = true;
        run.frame(vec![egui::Event::Text("Z".into())]);
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        assert!(run.app.tab().preview);
        run.click_tab("a.md");
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(run.app.tab().preview);
    }

    #[test]
    fn closing_the_front_tab_does_not_pin_the_preview_it_lands_on() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.sidebar.visible = true;
        run.frame(vec![egui::Event::Text("Z".into())]);
        run.key(egui::Key::Z, egui::Modifiers::COMMAND);
        assert!(!run.app.tab().doc.is_dirty());
        assert_ne!(run.app.tab().doc.epoch(), 0);
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        assert!(run.app.tab().preview);
        run.click_tab("a.md");
        run.key(egui::Key::W, egui::Modifiers::COMMAND);
        assert!(run.app.confirm.is_none());
        assert_eq!(run.app.tabs.len(), 1);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(run.app.tab().preview);
        assert_eq!(text(&run.app), "beta\n");
    }

    #[test]
    fn closing_a_background_tab_still_pins_an_edited_preview() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        fs::write(&c, "gamma\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.sidebar.visible = true;
        let at_c = run.row_center(&c);
        run.click_pos(at_c, egui::PointerButton::Primary);
        run.click_pos(at_c, egui::PointerButton::Primary);
        assert!(!run.app.tab().preview);
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        assert!(run.app.tab().preview);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        type_into(&mut run.app, "xx");
        assert!(run.app.tab().preview);
        let c_index = run
            .app
            .tabs
            .iter()
            .position(|tab| tab.doc.path() == Some(c.as_path()))
            .unwrap();
        run.app.remove_tab(c_index);
        run.frame(vec![]);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(!run.app.tab().preview);
        assert!(text(&run.app).ends_with("xx"));
        assert!(
            run.app
                .tabs
                .iter()
                .all(|tab| tab.doc.path() != Some(c.as_path()))
        );
    }

    #[test]
    fn a_search_hit_lands_in_the_preview_without_moving_the_other_caret() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha stays\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        fs::write(&c, "see token here\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.sidebar.visible = true;
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        run.click_tab("a.md");
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.app.jump_to(6);
        run.frame(vec![]);
        let kept = run.app.tab().code.selection();
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("token".into())]);
        run.wait_search();
        run.click_text("c.md  see token here");
        run.settle();
        assert!(run.app.tab().preview);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
        assert_eq!(
            run.app
                .tab()
                .doc
                .slice(run.app.tab().code.selection().range())
                .as_ref(),
            "token"
        );
        assert_eq!(run.app.tabs[0].doc.path(), Some(a.as_path()));
        assert_eq!(run.app.tabs[0].code.selection(), kept);
        assert!(
            run.app
                .tabs
                .iter()
                .all(|tab| tab.doc.path() != Some(b.as_path()))
        );
    }

    #[test]
    fn a_search_jump_survives_switching_away() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha stays\n").unwrap();
        fs::write(&c, "see token here\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.jump_to(6);
        run.frame(vec![]);
        let kept = run.app.tab().code.selection();
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("token".into())]);
        run.wait_search();
        let at = run
            .text_rect("c.md  see token here")
            .expect("the hit is on screen")
            .center();
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        run.frame(vec![egui::Event::PointerMoved(at), press(true)]);
        run.frame(vec![press(false)]);
        assert!(
            run.app.pending_jump.is_some(),
            "the open frame does not apply the jump"
        );
        run.app.activate(0);
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        assert_eq!(run.app.tab().code.selection(), kept);
        let opened = run
            .app
            .tabs
            .iter()
            .find(|tab| tab.doc.path() == Some(c.as_path()))
            .expect("the search hit stayed open");
        assert_eq!(
            opened.doc.slice(opened.code.selection().range()).as_ref(),
            "token"
        );
    }

    #[test]
    fn replacing_a_preview_drops_the_old_find_matches() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, format!("{}cat\n", "x".repeat(80))).unwrap();
        fs::write(&c, "no\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.sidebar.visible = true;
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        run.key(egui::Key::F, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("cat".into())]);
        run.frame(vec![]);
        assert_eq!(
            run.app
                .tab()
                .doc
                .slice(run.app.tab().code.selection().range())
                .as_ref(),
            "cat"
        );
        let at_c = run.row_center(&c);
        run.click_pos(at_c, egui::PointerButton::Primary);
        assert_eq!(text(&run.app), "no\n");
        run.key(egui::Key::F3, egui::Modifiers::NONE);
        assert_eq!(text(&run.app), "no\n");
        let range = run.app.tab().code.selection().range();
        assert!(range.end <= run.app.tab().doc.len());
    }

    #[test]
    fn an_open_find_bar_searches_the_file_that_replaces_a_preview() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "cat in b\n").unwrap();
        fs::write(&c, "see cat here\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.sidebar.visible = true;
        let at_b = run.row_center(&b);
        run.click_pos(at_b, egui::PointerButton::Primary);
        run.key(egui::Key::F, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("cat".into())]);
        run.frame(vec![]);
        assert_eq!(
            run.app
                .tab()
                .doc
                .slice(run.app.tab().code.selection().range())
                .as_ref(),
            "cat"
        );
        let at_c = run.row_center(&c);
        run.click_pos(at_c, egui::PointerButton::Primary);
        assert_eq!(text(&run.app), "see cat here\n");
        assert!(run.app.tab().find.match_count() > 0);
        assert_eq!(
            run.app
                .tab()
                .doc
                .slice(run.app.tab().code.selection().range())
                .as_ref(),
            "cat"
        );
    }

    #[test]
    fn ctrl_enter_and_a_middle_click_pin_a_search_hit() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        fs::write(&c, "gamma\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.sidebar.visible = true;
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("b".into())]);
        run.wait_search();
        run.key(egui::Key::Enter, egui::Modifiers::COMMAND);
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert!(!run.app.tab().preview);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("c".into())]);
        run.wait_search();
        run.click_text_button("c.md", egui::PointerButton::Middle);
        run.settle();
        assert_eq!(run.app.tabs.len(), 3);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
        assert!(!run.app.tab().preview);
        assert!(
            run.app
                .tabs
                .iter()
                .any(|tab| tab.doc.path() == Some(b.as_path()) && !tab.preview)
        );
    }

    #[test]
    fn enter_while_search_is_debouncing_does_not_open_the_old_hit() {
        let dir = tempfile::tempdir().unwrap();
        let start = dir.path().join("start.md");
        fs::write(&start, "start\n").unwrap();
        fs::write(dir.path().join("alpha.md"), "one\n").unwrap();
        fs::write(dir.path().join("beta.md"), "two\n").unwrap();
        let mut run = Run::new(dir.path(), Some(start.clone()));
        run.app.sidebar.visible = true;
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("alpha".into())]);
        run.wait_search();
        let at = run
            .text_rect("alpha.md")
            .expect("the old hit is still on screen")
            .center();
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        // The query change, the click on the old row, and Ctrl+Enter share
        // one frame, so the debounce clock cannot elapse between them.
        run.frame(vec![
            shortcut(egui::Key::A, egui::Modifiers::COMMAND),
            egui::Event::Text("beta".into()),
            egui::Event::PointerMoved(at),
            press(true),
            press(false),
            shortcut(egui::Key::Enter, egui::Modifiers::COMMAND),
        ]);
        assert_eq!(run.app.tab().doc.path(), Some(start.as_path()));
        assert_eq!(run.app.tabs.len(), 1);
        run.wait_search();
        // The click above leaves the query field, so Enter would not steer.
        // The new query's row is what opens once the debounce has elapsed.
        run.click_text("beta.md");
        run.settle();
        assert_eq!(
            run.app.tab().doc.path(),
            Some(dir.path().join("beta.md").as_path())
        );
        assert!(run.app.tab().preview);
    }

    #[test]
    fn a_match_in_an_edited_file_selects_the_moved_text() {
        let dir = tempfile::tempdir().unwrap();
        let beta = dir.path().join("beta.md");
        fs::write(&beta, "see token here\n").unwrap();
        let mut run = Run::new(dir.path(), Some(beta));
        run.app
            .tab_mut()
            .doc
            .apply(
                vec![inkmark_buffer::Edit::insert(0, "xx")],
                Selection::caret(0),
                Selection::caret(2),
                inkmark_buffer::EditKind::Other,
            )
            .unwrap();
        assert!(run.app.tab().doc.is_dirty());
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("token".into())]);
        run.wait_search();
        run.click_text("beta.md  see token here");
        assert_eq!(
            run.app
                .tab()
                .doc
                .slice(run.app.tab().code.selection().range())
                .as_ref(),
            "token"
        );
        assert_eq!(run.app.tab().code.selection().range(), 6..11);
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
            run.app
                .tab()
                .doc
                .slice(run.app.tab().code.selection().range())
                .as_ref(),
            "token"
        );
        assert_eq!(run.app.tab().code.selection().range(), 6..11);
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
        assert_eq!(run.app.tab().code.selection().range(), 12..15);
        assert_eq!(run.app.tab().find.match_index(), Some(2));
    }

    #[test]
    fn a_regex_find_selects_the_first_match_and_then_counts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "one quickly\ntwo slowly\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.key(egui::Key::F, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text(r"\w+ly".into())]);
        run.frame(vec![]);
        run.click_text("Regex");
        run.wait_find();
        assert_eq!(run.app.tab().code.selection().range(), 4..11);
        assert_eq!(run.app.tab().find.match_count(), 2);
        assert_eq!(run.app.tab().find.match_index(), Some(1));
        assert_eq!(
            run.app
                .tab()
                .doc
                .slice(run.app.tab().code.selection().range())
                .as_ref(),
            "quickly"
        );
    }

    #[test]
    fn closing_with_unsaved_changes_asks_and_cancel_keeps_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path.clone()));
        type_into(&mut r.app, "two\n");
        r.close_window();
        assert_eq!(r.app.confirm, Some(Confirm::Close(r.app.tabs[0].id)));
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
    fn closing_asks_about_a_dirty_tab_that_is_not_showing() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut r.app, "unsaved");
        r.app.request_open(b.clone());
        r.frame(vec![]);
        assert_eq!(r.app.tab().doc.path(), Some(b.as_path()));
        assert!(r.app.confirm.is_none());
        r.close_window();
        assert_eq!(r.app.confirm, Some(Confirm::Close(r.app.tabs[0].id)));
        assert_eq!(r.app.tab().doc.path(), Some(a.as_path()));
        assert!(r.sent(&ViewportCommand::CancelClose));
        r.click_text("Cancel");
        assert!(r.app.confirm.is_none());
        assert!(r.app.after_save.is_none());
        assert!(!r.sent(&ViewportCommand::Close));
        assert_eq!(tab_text(&r.app, 0), "a\nunsaved");
        r.app.save();
        r.frame(vec![]);
        assert!(!r.sent(&ViewportCommand::Close), "a later save stays open");
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\nunsaved");
    }

    #[test]
    fn closing_saves_each_dirty_tab_before_the_window_goes() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        fs::write(&c, "c\n").unwrap();

        let mut r = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut r.app, "A");
        r.app.request_open(b.clone());
        type_into(&mut r.app, "B");
        r.app.request_open(c.clone());
        type_into(&mut r.app, "C");
        r.close_window();
        assert_eq!(r.app.confirm, Some(Confirm::CloseAll));
        assert_eq!(
            r.app.dirty_labels(),
            vec![
                a.display().to_string(),
                b.display().to_string(),
                c.display().to_string()
            ]
        );
        r.click_text("Save all");
        assert!(r.sent(&ViewportCommand::Close));
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\nA");
        assert_eq!(fs::read_to_string(&b).unwrap(), "b\nB");
        assert_eq!(fs::read_to_string(&c).unwrap(), "c\nC");

        // Cancel stays, and a later save of one tab does not close the window.
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        fs::write(&c, "c\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut r.app, "A");
        r.app.request_open(b.clone());
        type_into(&mut r.app, "B");
        r.app.request_open(c.clone());
        type_into(&mut r.app, "C");
        r.close_window();
        r.frame(vec![]);
        r.click_text("Cancel");
        assert!(r.app.confirm.is_none());
        assert!(r.app.after_save.is_none());
        assert!(!r.sent(&ViewportCommand::Close));
        assert_eq!(tab_text(&r.app, 0), "a\nA");
        assert_eq!(tab_text(&r.app, 1), "b\nB");
        assert_eq!(tab_text(&r.app, 2), "c\nC");
        r.app.save();
        r.frame(vec![]);
        assert!(!r.sent(&ViewportCommand::Close));
        assert_eq!(fs::read_to_string(&c).unwrap(), "c\nC");
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\n");

        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        fs::write(&c, "c\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut r.app, "A");
        r.app.request_open(b.clone());
        type_into(&mut r.app, "B");
        r.app.request_open(c.clone());
        type_into(&mut r.app, "C");
        r.close_window();
        r.click_text("Discard all");
        assert!(r.sent(&ViewportCommand::Close));
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\n");
        assert_eq!(fs::read_to_string(&b).unwrap(), "b\n");
        assert_eq!(fs::read_to_string(&c).unwrap(), "c\n");
    }

    #[test]
    fn opening_another_file_keeps_the_unsaved_tab() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut r.app, "edit\n");
        r.app.request_open(b.clone());
        r.frame(vec![]);
        assert!(r.app.confirm.is_none());
        assert_eq!(r.app.tabs.len(), 2);
        assert_eq!(r.app.tab().doc.path(), Some(b.as_path()));
        assert_eq!(text(&r.app), "b\n");
        assert_eq!(tab_text(&r.app, 0), "a\nedit\n");
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\n");
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
        assert!(matches!(r.app.tab().banner, Some(Banner::DiskChanged)));
        r.click_text("Keep mine");
        assert!(r.app.tab().banner.is_none());
        r.check_disk_now();
        assert!(r.app.tab().banner.is_none(), "their version counts as seen");
        // Saving now writes ours over theirs, as chosen.
        r.app.save();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\nmine\n");

        fs::write(&path, "theirs again\n").unwrap();
        r.check_disk_now();
        r.click_text("Reload");
        assert_eq!(text(&r.app), "theirs again\n");
        assert!(r.app.tab().banner.is_none());
    }

    #[test]
    fn a_deleted_file_is_noticed_and_the_banner_dismissed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "one\n").unwrap();
        let mut r = Run::new(dir.path(), Some(path.clone()));
        fs::remove_file(&path).unwrap();
        r.check_disk_now();
        assert!(matches!(r.app.tab().banner, Some(Banner::DiskMissing)));
        r.click_text("Dismiss");
        assert!(r.app.tab().banner.is_none());
        r.check_disk_now();
        assert!(r.app.tab().banner.is_none(), "dismissed stays dismissed");
        // If a file appears there again, that's a change.
        fs::write(&path, "new\n").unwrap();
        r.check_disk_now();
        assert!(matches!(r.app.tab().banner, Some(Banner::DiskChanged)));
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
        assert_eq!(r.app.tab().doc.path(), Some(b.as_path()));
        // Escape closes it; a click on a row opens that file.
        r.key(egui::Key::R, egui::Modifiers::COMMAND);
        r.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(r.app.recent_list.is_none());
        r.key(egui::Key::R, egui::Modifiers::COMMAND);
        r.frame(vec![]);
        r.click_text("c.md");
        assert_eq!(r.app.tab().doc.path(), Some(c.as_path()));
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
        assert_eq!(app.tab().live.font_size, 20.0);
        assert_eq!(app.tab().code.font_size, CODE_SIZE);
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
            app.tab().live.font_size,
            20.0,
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
        assert_eq!(app.tab().live.line_height, (24.0 * TEXT_LINE).round());
        assert_eq!(app.tab().code.line_height, 21.0);

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
            assert_eq!(app.tab().live.font_size, 24.0);
        }
        // A deleted file means defaults.
        fs::remove_file(&config).unwrap();
        app.apply_settings_from(Some(&config), system);
        assert_eq!(app.tab().live.font_size, TEXT_SIZE);
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
        assert!(r.app.tab().code.show_minimap);
        r.key(
            egui::Key::M,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert!(r.app.tab().code.show_minimap);
        r.key(egui::Key::M, egui::Modifiers::COMMAND);
        assert!(!r.app.tab().code.show_minimap);

        r.app.focus_pane(&r.ctx, Pane::Live);
        r.key(
            egui::Key::Num1,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert!(r.app.tab().focus == Pane::Live);
        r.key(egui::Key::Num1, egui::Modifiers::COMMAND);
        assert!(r.app.tab().focus == Pane::Code);
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
            .tab_mut()
            .code
            .set_selection(inkmark_buffer::Selection { anchor: 0, head: 4 });
        r.app.tab_mut().code.request_focus(&r.ctx);
        r.key(egui::Key::E, egui::Modifiers::COMMAND);
        assert!(r.app.mode == Mode::Split, "cycle_mode was unbound");
        assert_eq!(text(&r.app), "**word**\n");
    }

    fn select_word(r: &mut Run) {
        let start = text(&r.app).find("word").unwrap();
        r.app
            .tab_mut()
            .code
            .set_selection(inkmark_buffer::Selection {
                anchor: start,
                head: start + 4,
            });
        r.app.tab_mut().code.request_focus(&r.ctx);
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
        assert_eq!(r.app.tab().live.font_size, 20.0);
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
        r.app.tab_mut().focus = Pane::Live;
        r.app.tab_mut().live.request_focus(&r.ctx);
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
        assert_eq!(app.tab().doc.path(), Some(a.as_path()));
    }

    #[test]
    fn history_changes_only_when_a_navigation_lands() {
        // Review of #33. History updates once the other file is on screen.
        // A place whose file is gone is refused and stays on the stack.
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "[next](b.md)\n").unwrap();
        fs::write(&b, "# B\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        let mut time = 0.0;
        settle(&ctx, &mut app, &mut time);
        type_into(&mut app, "draft");
        app.follow(1);
        assert!(app.confirm.is_none(), "opening another file keeps this tab");
        settle(&ctx, &mut app, &mut time);
        assert_eq!(app.tab().doc.path(), Some(b.as_path()));
        assert!(
            app.back
                .iter()
                .any(|place| place.path.as_deref() == Some(a.as_path())),
            "leaving a.md is recorded once b.md is open"
        );
        assert!(app.forward.is_empty());
        assert!(tab_text(&app, 0).contains("draft"));

        let gone = Place {
            path: Some(dir.path().join("gone.md")),
            offset: 0,
        };
        app.forward.push(gone.clone());
        let back = app.back.clone();
        app.go_forward();
        assert_eq!(app.forward, vec![gone]);
        assert_eq!(app.back, back);
        assert_eq!(app.tab().doc.path(), Some(b.as_path()));
        assert!(hint(&app).contains("gone.md"));
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
        assert_eq!(run.app.tab().code.selection(), Selection::caret(beta));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(beta));

        run.click_text("Alpha");
        assert_eq!(run.app.tab().code.selection(), Selection::caret(alpha));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(alpha));

        run.key(egui::Key::ArrowLeft, egui::Modifiers::ALT);
        assert_eq!(run.app.tab().code.selection(), Selection::caret(beta));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(beta));
        run.key(egui::Key::ArrowLeft, egui::Modifiers::ALT);
        assert_eq!(run.app.tab().code.selection(), Selection::caret(start));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(start));

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
        assert!(run.text_rect("Filter by name or key").is_some());
        assert!(run.text_rect("File").is_some());
        assert!(run.text_rect("Ctrl+O").is_some());
        assert!(run.text_rect("Open a file").is_some());
        assert!(run.text_rect("Ctrl+S").is_some());
        assert!(run.text_rect("Esc or Ctrl+/, F1 closes").is_some());
        assert_eq!(
            run.app.keys.shortcut_text(Action::ToggleOutline),
            "Ctrl+Shift+B"
        );
        assert_eq!(
            run.app.keys.shortcut_text(Action::NewFolder),
            "Ctrl+Shift+N"
        );
        assert_eq!(run.app.keys.shortcut_text(Action::ShowKeys), "Ctrl+/, F1");

        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
        assert!(run.text_rect("Key bindings").is_none());

        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(run.app.show_keys);
        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
    }

    #[test]
    fn the_key_overlay_filters_and_returns_focus() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "hello\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        assert!(run.app.tab().code.has_focus(&run.ctx));

        run.key(egui::Key::Slash, egui::Modifiers::COMMAND);
        assert!(run.app.show_keys);
        // A modal's first frame only measures. The list is painted after that.
        for _ in 0..3 {
            if run.text_rect("Key bindings").is_some() {
                break;
            }
            run.frame(vec![]);
        }
        assert!(run.text_rect("Key bindings").is_some());
        assert!(run.text_rect("Filter by name or key").is_some());
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("keys_filter"))),
            "the filter takes focus when the list opens"
        );
        // A later frame must not ask for focus again, and must not hand
        // the keyboard back to the pane while the list is open.
        run.frame(vec![]);
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("keys_filter"))),
            "the filter keeps focus"
        );
        assert!(!run.app.tab().code.has_focus(&run.ctx));
        assert!(run.text_rect("File").is_some());
        assert!(run.text_rect("Ctrl+O").is_some());
        assert!(run.text_rect("Open a file").is_some());
        // Tabs follows File. Edit and the later groups sit below the fold
        // until the filter brings them up.
        assert!(run.text_rect("Tabs").is_some());
        assert_eq!(text(&run.app), "hello\n");

        run.frame(vec![egui::Event::Text("reopen".into())]);
        assert_eq!(run.app.keys_query, "reopen");
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("keys_filter")))
        );
        assert_eq!(text(&run.app), "hello\n");
        assert!(run.text_rect("Tabs").is_some());
        assert!(run.text_rect("Reopen the closed tab").is_some());
        assert!(run.text_rect("Ctrl+Shift+T").is_some());
        assert!(run.text_rect("Toggle bold").is_none());
        assert!(run.text_rect("Edit").is_none());
        assert!(run.text_rect("File").is_none());

        run.app.keys_query = "cycle".into();
        run.frame(vec![]);
        assert!(run.text_rect("View/Layout").is_some());
        assert!(run.text_rect("Ctrl+E").is_some());
        assert!(run.text_rect("Cycle split, code, and live").is_some());
        assert!(run.text_rect("Reopen the closed tab").is_none());

        run.app.keys_query = "ctrl+click".into();
        run.frame(vec![]);
        assert!(run.text_rect("Minimap/Mouse").is_some());
        assert!(run.text_rect("Ctrl+click").is_some());
        assert!(run.text_rect("Follow a link, in a preview tab").is_some());

        run.app.keys_query = "middle".into();
        run.frame(vec![]);
        assert!(run.text_rect("Middle click a tab").is_some());
        assert!(run.text_rect("Middle click a link").is_some());
        assert!(run.text_rect("Middle click a search result").is_some());
        assert!(
            run.text_rect("Open the search result in a pinned tab")
                .is_some()
        );
        assert!(run.text_rect("Follow a link, in a pinned tab").is_some());

        // The same chord is two rows: the editor toggles a task, search pins.
        run.app.keys_query = "ctrl+enter".into();
        run.frame(vec![]);
        assert!(run.text_rect("Toggle the line's task checkbox").is_some());
        assert!(
            run.text_rect("Open the selected folder-search result in a pinned tab")
                .is_some()
        );
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("keys_filter")))
        );

        run.app.keys_query = "zzzzz".into();
        run.frame(vec![]);
        assert!(run.text_rect("No matching shortcuts").is_some());
        assert!(run.text_rect("Ctrl+O").is_none());

        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
        assert!(run.text_rect("Key bindings").is_none());
        assert!(run.app.tab().code.has_focus(&run.ctx));
        // The arrow stays in the pane, and the next character lands there
        // (CRO-128: a fresh focus request would drop the arrow lock).
        let before = run.app.tab().code.selection();
        run.key(egui::Key::ArrowRight, egui::Modifiers::NONE);
        assert!(run.app.tab().code.has_focus(&run.ctx));
        assert_ne!(run.app.tab().code.selection(), before);
        run.frame(vec![egui::Event::Text("Z".into())]);
        assert_eq!(text(&run.app), "hZello\n");
        assert!(run.app.tab().code.has_focus(&run.ctx));

        // Key text matches, and the same chord closes.
        run.key(egui::Key::Slash, egui::Modifiers::COMMAND);
        assert!(run.app.show_keys);
        assert!(run.app.keys_query.is_empty());
        for _ in 0..3 {
            if run.text_rect("Filter by name or key").is_some() {
                break;
            }
            run.frame(vec![]);
        }
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("keys_filter")))
        );
        run.frame(vec![egui::Event::Text("alt+1".into())]);
        assert!(run.text_rect("Switch to tab 1").is_some());
        assert!(run.text_rect("Toggle bold").is_none());
        assert!(run.text_rect("Reopen the closed tab").is_none());
        run.key(egui::Key::Slash, egui::Modifiers::COMMAND);
        assert!(!run.app.show_keys);
        assert!(run.app.tab().code.has_focus(&run.ctx));

        // The live pane gets its own focus back, lock included.
        run.app.focus_pane(&run.ctx, Pane::Live);
        run.frame(vec![]);
        assert!(run.app.tab().live.has_focus(&run.ctx));
        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(run.app.show_keys);
        run.frame(vec![]);
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("keys_filter")))
        );
        assert!(!run.app.tab().live.has_focus(&run.ctx));
        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
        assert!(run.app.tab().live.has_focus(&run.ctx));
        let before = run.app.tab().live.selection();
        run.key(egui::Key::ArrowRight, egui::Modifiers::NONE);
        assert!(run.app.tab().live.has_focus(&run.ctx));
        assert_ne!(run.app.tab().live.selection(), before);
        run.frame(vec![egui::Event::Text("Q".into())]);
        assert!(text(&run.app).contains('Q'));
        assert!(run.app.tab().live.has_focus(&run.ctx));
    }

    /// Escape closes the list even when config binds it to an app action.
    /// That action must not run: the key is left for the overlay.
    #[test]
    fn escape_closes_the_overlay_when_save_is_bound_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "hello\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path.clone()));
        run.frame(vec![egui::Event::Text("X".into())]);
        assert!(run.app.tab().doc.is_dirty());
        run.app
            .keys
            .set(Action::Save, vec![keys::Chord::parse("Escape").unwrap()]);
        run.app.install_keys();
        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(run.app.show_keys);
        for _ in 0..3 {
            if run.text_rect("Key bindings").is_some() {
                break;
            }
            run.frame(vec![]);
        }
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("keys_filter")))
        );
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
        assert!(run.app.tab().doc.is_dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
        assert!(run.app.error.is_none());
        assert!(run.app.tab().code.has_focus(&run.ctx));
    }

    /// The e2e window is 480×320. The list has to fit that, not a fixed 640×420.
    #[test]
    fn the_key_overlay_fits_the_e2e_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "hello\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        let size = egui::vec2(480.0, 320.0);
        run.frame_sized(vec![shortcut(egui::Key::F1, egui::Modifiers::NONE)], size);
        assert!(run.app.show_keys);
        for _ in 0..4 {
            run.frame_sized(vec![], size);
        }
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        for label in [
            "Key bindings",
            "Filter by name or key",
            "Esc or Ctrl+/, F1 closes",
        ] {
            let rect = run
                .text_rect(label)
                .unwrap_or_else(|| panic!("{label} isn't on screen"));
            assert!(
                screen.contains_rect(rect),
                "{label} {rect:?} falls outside {screen:?}"
            );
        }
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("keys_filter")))
        );
    }

    /// Ctrl+Shift+/ opens the cheatsheet. Escape and the same chord close
    /// it, and the pane that had the keyboard gets it back, arrow lock
    /// included (CRO-128). A click inserts the row and leaves the sheet open.
    #[test]
    fn the_syntax_cheatsheet_opens_closes_and_returns_focus() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "hello\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path.clone()));
        assert!(run.app.tab().code.has_focus(&run.ctx));

        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::Slash, shift);
        assert!(run.app.show_syntax);
        assert!(!run.app.show_keys);
        for _ in 0..4 {
            let painted = run.text_rect("Markdown syntax").is_some()
                && run
                    .app
                    .syntax
                    .row_rects()
                    .first()
                    .is_some_and(|rect| rect.height() > 8.0);
            let focused = run
                .ctx
                .memory(|m| m.has_focus(egui::Id::new("syntax_sheet")));
            if painted && focused {
                break;
            }
            run.frame(vec![]);
        }
        assert!(run.text_rect("Markdown syntax").is_some());
        assert!(run.text_rect("Syntax").is_some());
        assert!(run.text_rect("Rendered").is_some());
        assert!(
            run.text_rect("Esc or Ctrl+Shift+/ closes. Click a row to insert it.")
                .is_some()
        );
        assert!(
            run.ctx
                .memory(|m| m.has_focus(egui::Id::new("syntax_sheet"))),
            "the cheatsheet takes focus"
        );
        assert!(!run.app.tab().code.has_focus(&run.ctx));
        assert!(
            run.app
                .syntax
                .row_rects()
                .first()
                .is_some_and(|rect| rect.height() > 8.0),
            "a row was drawn"
        );

        // Keys behind the sheet do not edit the note.
        run.frame(vec![egui::Event::Text("Z".into())]);
        assert_eq!(text(&run.app), "hello\n");

        let row = run.app.syntax.row_rects()[0];
        run.click_pos(row.center(), egui::PointerButton::Primary);
        assert!(
            run.app.show_syntax,
            "a click inserts and leaves the sheet open"
        );
        assert!(
            text(&run.app).starts_with("# Heading\n"),
            "{}",
            text(&run.app)
        );
        assert!(text(&run.app).contains("hello"));

        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_syntax);
        assert!(run.text_rect("Markdown syntax").is_none());
        assert!(run.app.tab().code.has_focus(&run.ctx));
        let before = run.app.tab().code.selection();
        run.key(egui::Key::ArrowRight, egui::Modifiers::NONE);
        assert!(run.app.tab().code.has_focus(&run.ctx));
        assert_ne!(run.app.tab().code.selection(), before);
        run.frame(vec![egui::Event::Text("Z".into())]);
        assert!(text(&run.app).contains('Z'));
        assert!(run.app.tab().code.has_focus(&run.ctx));

        // The same chord closes, and focus comes back.
        run.key(egui::Key::Slash, shift);
        assert!(run.app.show_syntax);
        run.key(egui::Key::Slash, shift);
        assert!(!run.app.show_syntax);
        assert!(run.app.tab().code.has_focus(&run.ctx));

        // The live pane gets its own focus back, lock included.
        run.app.focus_pane(&run.ctx, Pane::Live);
        run.frame(vec![]);
        assert!(run.app.tab().live.has_focus(&run.ctx));
        run.key(egui::Key::Slash, shift);
        assert!(run.app.show_syntax);
        for _ in 0..4 {
            if run
                .ctx
                .memory(|m| m.has_focus(egui::Id::new("syntax_sheet")))
            {
                break;
            }
            run.frame(vec![]);
        }
        assert!(!run.app.tab().live.has_focus(&run.ctx));
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_syntax);
        assert!(run.app.tab().live.has_focus(&run.ctx));
        let before = run.app.tab().live.selection();
        run.key(egui::Key::ArrowRight, egui::Modifiers::NONE);
        assert!(run.app.tab().live.has_focus(&run.ctx));
        assert_ne!(run.app.tab().live.selection(), before);
        run.frame(vec![egui::Event::Text("Q".into())]);
        assert!(text(&run.app).contains('Q'));
        assert!(run.app.tab().live.has_focus(&run.ctx));

        // Listed in the key-binding overlay.
        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(run.app.show_keys);
        for _ in 0..3 {
            if run.text_rect("Filter by name or key").is_some() {
                break;
            }
            run.frame(vec![]);
        }
        run.frame(vec![egui::Event::Text("cheatsheet".into())]);
        assert!(
            run.text_rect("Show or hide the Markdown syntax cheatsheet")
                .is_some()
        );
        assert!(run.text_rect("Ctrl+Shift+/").is_some());
        assert!(run.text_rect("Toggle bold").is_none());
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
        assert!(run.app.tab().live.has_focus(&run.ctx));

        // Escape closes the sheet and does not Save, even when Save is Escape.
        run.app.focus_pane(&run.ctx, Pane::Code);
        run.frame(vec![]);
        run.app
            .keys
            .set(Action::Save, vec![keys::Chord::parse("Escape").unwrap()]);
        run.app.install_keys();
        run.key(egui::Key::Slash, shift);
        assert!(run.app.show_syntax);
        for _ in 0..3 {
            if run.text_rect("Markdown syntax").is_some() {
                break;
            }
            run.frame(vec![]);
        }
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_syntax);
        assert!(run.app.tab().doc.is_dirty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
        assert!(text(&run.app).contains("# Heading"));
        assert!(run.app.tab().code.has_focus(&run.ctx));
    }

    /// The e2e window is 480×320. The cheatsheet has to fit that.
    #[test]
    fn the_syntax_cheatsheet_fits_the_e2e_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "hello\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        let size = egui::vec2(480.0, 320.0);
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.frame_sized(vec![shortcut(egui::Key::Slash, shift)], size);
        assert!(run.app.show_syntax);
        for _ in 0..4 {
            run.frame_sized(vec![], size);
        }
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        for label in [
            "Markdown syntax",
            "Syntax",
            "Rendered",
            "Esc or Ctrl+Shift+/ closes. Click a row to insert it.",
        ] {
            let rect = run
                .text_rect(label)
                .unwrap_or_else(|| panic!("{label} isn't on screen"));
            assert!(
                screen.contains_rect(rect),
                "{label} {rect:?} falls outside {screen:?}"
            );
        }
    }

    /// Folder search reads `pin_search_result`. Rebinding it changes the
    /// chord the field consumes, and the overlay shows that same chord.
    #[test]
    fn folder_search_reads_the_pin_binding() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha.md");
        let beta = dir.path().join("beta.md");
        let gamma = dir.path().join("gamma.md");
        fs::write(&alpha, "hello\n").unwrap();
        fs::write(&beta, "token\n").unwrap();
        fs::write(&gamma, "other\n").unwrap();
        let mut run = Run::new(dir.path(), Some(alpha));
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("beta".into())]);
        run.wait_search();
        run.key(egui::Key::Enter, egui::Modifiers::COMMAND);
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(beta.as_path()));
        assert!(!run.app.tab().preview);
        assert_eq!(text(&run.app), "token\n");

        run.app.keys.set(
            Action::PinSearchResult,
            vec![keys::Chord::parse("Ctrl+P").unwrap()],
        );
        run.app.install_keys();
        assert_eq!(
            run.app.keys.shortcut_text(Action::PinSearchResult),
            "Ctrl+P"
        );
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("gamma".into())]);
        run.wait_search();
        assert!(
            run.text_rect("gamma.md").is_some(),
            "the new query replaced the old one"
        );
        run.key(egui::Key::Enter, egui::Modifiers::COMMAND);
        assert!(
            run.app
                .tabs
                .iter()
                .all(|tab| tab.doc.path() != Some(gamma.as_path()))
        );
        assert_eq!(run.app.tab().doc.path(), Some(beta.as_path()));
        assert_eq!(text(&run.app), "token\n");
        run.key(egui::Key::P, egui::Modifiers::COMMAND);
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(gamma.as_path()));
        assert!(!run.app.tab().preview);
        assert_eq!(text(&run.app), "other\n");

        run.key(egui::Key::F1, egui::Modifiers::NONE);
        for _ in 0..3 {
            if run.text_rect("Filter by name or key").is_some() {
                break;
            }
            run.frame(vec![]);
        }
        run.frame(vec![egui::Event::Text("ctrl+p".into())]);
        assert!(
            run.text_rect("Open the selected folder-search result in a pinned tab")
                .is_some()
        );
        assert!(run.text_rect("Ctrl+P").is_some());
        assert!(run.text_rect("Toggle the line's task checkbox").is_none());
    }

    /// Enter opens an unpinned preview. Binding that key, or Escape, to
    /// `pin_search_result` runs the binding. The hardcoded key must not
    /// take the event first.
    #[test]
    fn search_bindings_on_enter_and_escape_are_not_shadowed() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha.md");
        let beta = dir.path().join("beta.md");
        let gamma = dir.path().join("gamma.md");
        fs::write(&alpha, "hello\n").unwrap();
        fs::write(&beta, "beta token\n").unwrap();
        fs::write(&gamma, "gamma token\n").unwrap();
        let mut run = Run::new(dir.path(), Some(alpha));
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);

        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("beta".into())]);
        run.wait_search();
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(beta.as_path()));
        assert!(run.app.tab().preview);
        assert!(run.app.search.is_open());
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.search.is_open());
        assert_eq!(run.app.tab().doc.path(), Some(beta.as_path()));

        let applied = keys::KeyMap::apply(&[("pin_search_result".into(), vec!["Enter".into()])]);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        run.app.keys = applied.map;
        run.app.install_keys();
        assert_eq!(run.app.keys.shortcut_text(Action::PinSearchResult), "Enter");
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("gamma".into())]);
        run.wait_search();
        assert!(run.text_rect("gamma.md").is_some());
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(gamma.as_path()));
        assert!(
            !run.app.tab().preview,
            "Enter is the pin chord, so the hit is not a preview"
        );
        assert!(run.app.search.is_open());

        run.key(egui::Key::F1, egui::Modifiers::NONE);
        for _ in 0..3 {
            if run.text_rect("Filter by name or key").is_some() {
                break;
            }
            run.frame(vec![]);
        }
        run.frame(vec![egui::Event::Text("pinned tab".into())]);
        assert!(
            run.text_rect("Open the selected folder-search result in a pinned tab")
                .is_some()
        );
        assert!(run.text_rect("Enter").is_some());
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);

        let applied = keys::KeyMap::apply(&[("pin_search_result".into(), vec!["Escape".into()])]);
        assert!(applied.errors.is_empty(), "{:?}", applied.errors);
        run.app.keys = applied.map;
        run.app.install_keys();
        assert_eq!(
            run.app.keys.shortcut_text(Action::PinSearchResult),
            "Escape"
        );
        run.key(egui::Key::F, shift);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("beta".into())]);
        run.wait_search();
        assert!(run.text_rect("beta.md").is_some());
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        run.settle();
        assert!(
            run.app.search.is_open(),
            "Escape is the pin chord, so it does not close the bar"
        );
        assert_eq!(run.app.tab().doc.path(), Some(beta.as_path()));
        assert!(!run.app.tab().preview);
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
        let found = inkmark_parse::headings(&run.app.tab().doc, run.app.tab().parse.output());
        assert_eq!(found[0].text, "");
        run.click_text("Empty heading");
        assert_eq!(
            run.app.tab().code.selection(),
            Selection::caret(found[0].offset)
        );
        assert_eq!(
            run.app.tab().live.selection(),
            Selection::caret(found[0].offset)
        );
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
        assert_eq!(run.app.tab().code.selection(), Selection::caret(two));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(two));
        assert!(run.text_rect("Go to line").is_none());

        run.key(egui::Key::ArrowLeft, egui::Modifiers::ALT);
        assert_eq!(run.app.tab().code.selection(), Selection::caret(0));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(0));

        run.key(egui::Key::G, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("99".into())]);
        run.frame(vec![]);
        run.click_text("Go");
        run.frame(vec![]);
        assert_eq!(run.app.tab().code.selection(), Selection::caret(three));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(three));
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
        assert_eq!(run.app.tab().code.selection().head, 0);
        assert!(run.text_rect("Go to line").is_some());
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert!(run.text_rect("Go to line").is_none());
        assert_eq!(run.app.tab().code.selection().head, 0);
        assert_eq!(text(&run.app), "one\ntwo\n");
    }

    #[test]
    fn folding_a_heading_hides_its_body_in_the_code_pane() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        let src = "# Alpha\nvisible body\n## Beta\ninner body\n# Gamma\ntail\n";
        fs::write(&path, src).unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        assert!(run.app.tab().code.measured_height(1) > 0.0);
        assert!(run.app.tab().live.measured_height(1) > 0.0);

        run.click_code_mark("▾");
        let hidden = run.app.tab().code.hidden_ranges().to_vec();
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        let body = text(&run.app).find("visible body").unwrap();
        let inner = text(&run.app).find("inner body").unwrap();
        let gamma = text(&run.app).find("# Gamma").unwrap();
        assert!(hidden[0].start <= body && inner < hidden[0].end);
        assert_eq!(hidden[0].end, gamma);
        assert_eq!(run.app.tab().code.measured_height(1), 0.0);
        assert!(run.app.tab().live.measured_height(1) > 0.0);
        assert_eq!(text(&run.app), src);

        run.app.tab().code.request_focus(&run.ctx);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text("\n".into())]);
        run.frame(vec![]);
        let shifted = run.app.tab().code.hidden_ranges().to_vec();
        assert_eq!(shifted.len(), 1, "{shifted:?}");
        assert_eq!(shifted[0].start, hidden[0].start + 1);
        assert_eq!(shifted[0].end, hidden[0].end + 1);

        let inner = text(&run.app).find("inner body").unwrap();
        let line = run.app.tab().doc.byte_to_line(inner) + 1;
        run.key(egui::Key::G, egui::Modifiers::COMMAND);
        run.frame(vec![]);
        run.frame(vec![egui::Event::Text(line.to_string())]);
        run.frame(vec![]);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert_eq!(run.app.tab().code.selection(), Selection::caret(inner));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(inner));
        assert!(
            run.app
                .tab()
                .code
                .hidden_ranges()
                .iter()
                .all(|range| inner < range.start || inner >= range.end),
            "{:?}",
            run.app.tab().code.hidden_ranges()
        );
        let shown = run.app.tab().doc.byte_to_line(inner);
        assert!(run.app.tab().code.measured_height(shown) > 0.0);
        assert!(run.app.tab().live.measured_height(shown) > 0.0);

        run.click_code_mark("▾");
        assert!(!run.app.tab().code.hidden_ranges().is_empty());
        let other = dir.path().join("other.md");
        fs::write(&other, "plain\n").unwrap();
        run.app.open(other);
        run.frame(vec![]);
        assert!(run.app.tab().code.hidden_ranges().is_empty());
    }

    #[test]
    fn enter_above_a_fold_keeps_it_and_shows_the_new_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Alpha\nbody line\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.click_code_mark("▾");
        run.app.tab().code.request_focus(&run.ctx);
        run.frame(vec![]);
        run.key(egui::Key::End, egui::Modifiers::NONE);
        run.key(egui::Key::Enter, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert_eq!(text(&run.app), "# Alpha\n\nbody line\n");
        let hidden = run.app.tab().code.hidden_ranges().to_vec();
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        assert!(
            run.app.tab().code.measured_height(1) > 0.0,
            "the new line is drawn"
        );
        assert_eq!(run.app.tab().code.measured_height(2), 0.0);
        let in_code = |rect: &egui::Rect| rect.left() > 220.0 && rect.left() < 520.0;
        assert!(
            run.text_rects("▸").iter().any(in_code),
            "the heading still shows the fold"
        );
        run.click_code_mark("▸");
        assert!(run.app.tab().code.hidden_ranges().is_empty());
    }

    #[test]
    fn arrow_down_stops_on_the_last_visible_line_of_a_fold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Last\nhidden body").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.click_code_mark("▾");
        let hidden = run.app.tab().code.hidden_ranges().to_vec();
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        run.app.tab().code.request_focus(&run.ctx);
        run.frame(vec![]);
        run.key(egui::Key::ArrowDown, egui::Modifiers::NONE);
        run.frame(vec![]);
        assert_eq!(run.app.tab().code.selection().head, 0);
        assert_eq!(run.app.tab().code.hidden_ranges(), hidden.as_slice());
        assert_eq!(run.app.tab().code.measured_height(1), 0.0);
        assert_eq!(text(&run.app), "# Last\nhidden body");
    }

    #[test]
    fn a_jump_after_a_live_edit_rebases_folds_before_revealing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        fs::write(&path, "# Alpha\nbody line\n# Gamma\ntail\n").unwrap();
        let mut run = Run::new(dir.path(), Some(path));
        run.click_code_mark("▾");
        let start = run.app.tab().code.hidden_ranges()[0].start;
        let end = run.app.tab().code.hidden_ranges()[0].end;
        // The code pane is not on screen, so it has not shifted the fold.
        run.app.mode = Mode::Live;
        let inserted = "xxxxxxxxxxxxxxxxxxxxxxxx";
        run.app
            .tab_mut()
            .doc
            .apply(
                vec![inkmark_buffer::Edit::insert(0, inserted)],
                Selection::caret(0),
                Selection::caret(inserted.len()),
                inkmark_buffer::EditKind::Other,
            )
            .unwrap();
        // Inside the old fold range, outside the range once it shifts.
        run.app
            .tab_mut()
            .code
            .set_selection(Selection::caret(start));
        run.app.mode = Mode::Code;
        run.frame(vec![]);
        let hidden = run.app.tab().code.hidden_ranges().to_vec();
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
        let word = run.app.tab().code.selection().range();
        assert_eq!(&text(&run.app)[word.clone()], "one");
        assert_eq!(run.app.tab().live.selection().range(), word);

        run.key(
            egui::Key::P,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        let para = run.app.tab().code.selection().range();
        assert_eq!(&text(&run.app)[para.clone()], "one two\n");
        assert_eq!(run.app.tab().live.selection().range(), para);

        let open = text(&run.app).find('[').unwrap();
        let close = text(&run.app).find(']').unwrap();
        run.app.tab_mut().code.set_selection(Selection::caret(open));
        run.frame(vec![]);
        run.key(
            egui::Key::Backslash,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert_eq!(run.app.tab().code.selection(), Selection::caret(close));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(close));
        run.key(
            egui::Key::Backslash,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert_eq!(run.app.tab().code.selection(), Selection::caret(open));

        // The live pane swallows the chord and leaves the caret where it is.
        run.app.focus_pane(&run.ctx, Pane::Live);
        run.frame(vec![]);
        run.key(egui::Key::D, egui::Modifiers::COMMAND);
        run.key(
            egui::Key::Backslash,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert_eq!(text(&run.app), src);
        assert_eq!(run.app.tab().live.selection(), Selection::caret(open));
        assert_eq!(run.app.tab().code.selection(), Selection::caret(open));
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
                .tab()
                .code
                .hidden_ranges()
                .iter()
                .any(|range| range.start <= open && close < range.end),
            "{:?}",
            run.app.tab().code.hidden_ranges()
        );

        run.app.tab().code.request_focus(&run.ctx);
        run.app
            .tab_mut()
            .code
            .mirror_selection(Selection::caret(open));
        run.key(
            egui::Key::Backslash,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        run.frame(vec![]);
        assert_eq!(run.app.tab().code.selection(), Selection::caret(close));
        assert_eq!(run.app.tab().live.selection(), Selection::caret(close));
        assert!(
            run.app
                .tab()
                .code
                .hidden_ranges()
                .iter()
                .all(|range| close < range.start || close >= range.end),
            "{:?}",
            run.app.tab().code.hidden_ranges()
        );
        let line = run.app.tab().doc.byte_to_line(close);
        assert!(run.app.tab().code.measured_height(line) > 0.0);
        assert_eq!(text(&run.app), src);
    }

    fn hint(app: &App) -> String {
        app.hint
            .as_ref()
            .map(|(text, _)| text.clone())
            .unwrap_or_default()
    }

    #[test]
    fn the_strip_is_hidden_until_a_second_tab_opens() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        assert_eq!(run.app.tabs.len(), 1);
        assert_eq!(run.text_rects("a.md").len(), 1, "one tab draws no strip");
        run.app.request_open(b.clone());
        run.settle();
        assert_eq!(run.app.tabs.len(), 2);
        assert!(
            run.text_rects("a.md").len() >= 2,
            "the open file and its tab are both drawn"
        );
        assert!(run.text_rects("b.md").len() >= 2);
    }

    #[test]
    fn the_same_file_is_not_opened_twice() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, "a\n").unwrap();
        let via = dir.path().join("sub/../a.md");
        let mut app = app(dir.path(), Some(path.clone()));
        app.request_open(via);
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tab().doc.path(), Some(path.as_path()));
    }

    #[test]
    fn a_new_tab_is_built_with_the_current_settings() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut app = app(dir.path(), Some(a));
        app.tab_mut().code.font_size = 18.0;
        app.tab_mut().code.line_height = 30.0;
        app.tab_mut().code.show_minimap = false;
        app.tab_mut().live.font_size = 19.0;
        app.tab_mut().live.line_height = 31.0;
        app.tab_mut().live.show_minimap = false;
        app.keys.set(
            Action::Bold,
            vec![keys::Chord::parse("Ctrl+Shift+U").unwrap()],
        );
        app.install_keys();
        app.open(b);
        assert_eq!(app.tabs.len(), 2);
        let new = app.tab();
        assert_eq!(new.code.font_size, 18.0);
        assert_eq!(new.code.line_height, 30.0);
        assert!(!new.code.show_minimap);
        assert_eq!(new.live.font_size, 19.0);
        assert_eq!(new.live.line_height, 31.0);
        assert!(!new.live.show_minimap);
        assert_eq!(new.code.keys(), &app.keys);
        assert_eq!(new.live.keys(), &app.keys);
        assert_eq!(new.code.keys().shortcut_text(Action::Bold), "Ctrl+Shift+U");
        assert_eq!(
            app.tabs[0].code.keys().shortcut_text(Action::Bold),
            "Ctrl+Shift+U"
        );
        assert_ne!(app.tabs[0].code.id(), app.tabs[1].code.id());
        assert_ne!(app.tabs[0].live.id(), app.tabs[1].live.id());
        assert_ne!(app.tabs[0].code.id(), app.tabs[0].live.id());
        assert!(!new.preview);
    }

    #[test]
    fn switching_tabs_keeps_undo_caret_scroll_and_typed_text() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let original = "line\n".repeat(80);
        let at = 60 * "line\n".len();
        fs::write(&a, &original).unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.jump_to(at);
        for _ in 0..4 {
            run.frame(vec![]);
        }
        run.frame(vec![egui::Event::Text("A".into())]);
        for _ in 0..4 {
            run.frame(vec![]);
        }
        let caret = run.app.tab().code.selection();
        assert_eq!(caret, Selection::caret(at + 1));
        let line = run.app.tab().code.scroll_pos().line;
        assert!(line > 0, "the caret sits below the first screen");

        run.app.request_open(b.clone());
        run.settle();
        run.app.jump_to(text(&run.app).len());
        run.frame(vec![egui::Event::Text("B".into())]);
        assert_eq!(text(&run.app), "b\nB");
        assert!(run.app.tab().code.has_focus(&run.ctx));

        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        assert!(run.app.tab().code.has_focus(&run.ctx));
        assert_eq!(run.app.tab().code.selection(), caret);
        assert_eq!(run.app.tab().code.scroll_pos().line, line);
        run.frame(vec![egui::Event::Text("Z".into())]);
        let mut edited = original.clone();
        edited.insert(at, 'A');
        edited.insert(at + 1, 'Z');
        assert_eq!(text(&run.app), edited);
        assert_eq!(tab_text(&run.app, 1), "b\nB");

        run.key(egui::Key::Z, egui::Modifiers::COMMAND);
        run.key(egui::Key::Z, egui::Modifiers::COMMAND);
        assert_eq!(text(&run.app), original);
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(text(&run.app), "b\nB");
        assert!(run.app.tab().doc.can_undo());
    }

    #[test]
    fn tab_keys_follow_mru_and_the_strip_and_a_click_returns_focus() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        fs::write(&c, "c\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.request_open(b.clone());
        run.app.request_open(c.clone());
        run.settle();
        run.app.sidebar.visible = false;
        run.frame(vec![]);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));

        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.key(
            egui::Key::Tab,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
        );
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));

        run.key(egui::Key::PageDown, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
        run.key(egui::Key::PageDown, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.key(egui::Key::PageUp, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));

        run.key(egui::Key::Num1, egui::Modifiers::ALT);
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.key(egui::Key::Num3, egui::Modifiers::ALT);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
        run.key(egui::Key::Num9, egui::Modifiers::ALT);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));

        run.click_text("a.md");
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        assert!(run.app.tab().code.has_focus(&run.ctx));
        // A new tab's caret is at the start. Move it, then type there.
        run.app.jump_to(text(&run.app).len());
        run.frame(vec![egui::Event::Text("Q".into())]);
        assert_eq!(text(&run.app), "a\nQ");
        assert_eq!(tab_text(&run.app, 1), "b\n");
        assert_eq!(tab_text(&run.app, 2), "c\n");
    }

    #[test]
    fn releasing_ctrl_makes_the_landed_tab_most_recent() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        fs::write(&c, "c\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.request_open(b.clone());
        run.app.request_open(c.clone());
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));

        // One press, then Ctrl up: the next press returns to the tab we left.
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        run.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
        run.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);

        // Held across two presses, the walk continues past b. Releasing
        // lands on a, and the next press returns to where that walk started.
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
        run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tab().doc.path(), Some(c.as_path()));
    }

    #[test]
    fn closing_a_dirty_tab_asks_and_the_last_tab_leaves_the_recent_list() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.request_open(b.clone());
        run.settle();
        run.app.sidebar.visible = false;
        run.frame(vec![]);
        type_into(&mut run.app, "dirty");
        run.key(egui::Key::W, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tabs.len(), 2);
        assert_eq!(run.app.confirm, Some(Confirm::CloseTab(run.app.tabs[1].id)));
        assert!(text(&run.app).ends_with("dirty"));
        run.click_text("Cancel");
        assert!(run.app.confirm.is_none());
        assert_eq!(run.app.tabs.len(), 2);
        assert!(run.app.after_save.is_none());

        run.click_text_button("a.md", egui::PointerButton::Middle);
        assert_eq!(run.app.tabs.len(), 1);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        run.key(egui::Key::W, egui::Modifiers::COMMAND);
        assert_eq!(run.app.tabs.len(), 1);
        assert_eq!(run.app.confirm, Some(Confirm::CloseTab(run.app.tabs[0].id)));
        run.click_text("Discard");
        assert!(run.app.tabs.is_empty());
        assert!(run.app.recent_list.is_none());
        assert!(!run.sent(&ViewportCommand::Close));
        run.frame(vec![]);
        assert!(run.text_rect("Recent files").is_some());
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(run.app.recent_list.is_none());
        assert!(run.app.tabs.is_empty());
        assert!(!run.sent(&ViewportCommand::Close));
        // The list is in the pane, so F1 can open the key list over it.
        run.key(egui::Key::F1, egui::Modifiers::NONE);
        assert!(run.app.show_keys);
        run.key(egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!run.app.show_keys);
        assert!(run.app.recent_list.is_none());
        run.frame(vec![]);
        assert!(run.text_rect("Recent files").is_some());
        run.key(egui::Key::R, egui::Modifiers::COMMAND);
        assert!(run.app.recent_list.is_none());
        run.click_text("a.md");
        assert_eq!(run.app.tabs.len(), 1);
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
    }

    #[test]
    fn closing_the_last_tab_lets_the_sidebar_open_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.request_open(b.clone());
        run.settle();
        run.app.sidebar.visible = true;
        run.key(egui::Key::W, egui::Modifiers::COMMAND);
        run.key(egui::Key::W, egui::Modifiers::COMMAND);
        assert!(run.app.tabs.is_empty());
        assert!(run.app.recent_list.is_none());
        let start = Instant::now();
        let at = loop {
            run.frame(vec![]);
            if let Some(row) = run.app.browser.row_rect(&b) {
                break row.center();
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "sidebar listing did not arrive"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let press = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        run.frame(vec![egui::Event::PointerMoved(at), press(true)]);
        run.frame(vec![press(false)]);
        assert_eq!(run.app.tabs.len(), 1);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
    }

    #[test]
    fn closing_a_clean_window_does_not_ask() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a));
        r.app.request_open(b);
        r.frame(vec![]);
        r.close_window();
        assert!(r.app.confirm.is_none());
        assert!(!r.sent(&ViewportCommand::CancelClose));
        assert!(!r.sent(&ViewportCommand::Close));
    }

    #[test]
    fn closing_a_dirty_tab_saves_that_tab_only() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut r.app, "A");
        r.app.request_open(b.clone());
        type_into(&mut r.app, "B");
        r.frame(vec![]);
        r.key(egui::Key::W, egui::Modifiers::COMMAND);
        assert_eq!(r.app.confirm, Some(Confirm::CloseTab(r.app.tabs[1].id)));
        r.click_text("Save");
        assert_eq!(r.app.tabs.len(), 1);
        assert_eq!(r.app.tab().doc.path(), Some(a.as_path()));
        assert_eq!(fs::read_to_string(&b).unwrap(), "b\nB");
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\n");
        assert_eq!(tab_text(&r.app, 0), "a\nA");
        assert!(!r.sent(&ViewportCommand::Close));
    }

    #[test]
    fn save_all_stops_when_a_file_changed_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut r.app, "A");
        r.app.request_open(b.clone());
        type_into(&mut r.app, "B");
        fs::write(&b, "theirs\n").unwrap();
        r.close_window();
        assert_eq!(r.app.confirm, Some(Confirm::CloseAll));
        r.click_text("Save all");
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\nA");
        assert_eq!(fs::read_to_string(&b).unwrap(), "theirs\n");
        assert!(matches!(r.app.tab().banner, Some(Banner::DiskChanged)));
        assert_eq!(r.app.tab().doc.path(), Some(b.as_path()));
        assert_eq!(text(&r.app), "b\nB");
        assert!(r.app.after_save.is_none());
        assert!(!r.sent(&ViewportCommand::Close));
        r.app.save();
        r.frame(vec![]);
        assert!(!r.sent(&ViewportCommand::Close));
        assert_eq!(fs::read_to_string(&b).unwrap(), "theirs\n");
    }

    #[test]
    fn save_as_refuses_a_path_another_tab_has_open() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut app = app(dir.path(), Some(a.clone()));
        app.request_open(b.clone());
        type_into(&mut app, "B");
        app.save_as(a.clone());
        assert!(
            app.error.as_deref().unwrap().contains("already open"),
            "{:?}",
            app.error
        );
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\n");
        assert_eq!(text(&app), "b\nB");
        assert!(app.after_save.is_none());
        // The tab's own path is still a normal save.
        app.save_as(b.clone());
        assert_eq!(fs::read_to_string(&b).unwrap(), "b\nB");
        assert!(app.error.is_none());
    }

    #[test]
    fn renaming_a_folder_follows_both_open_files() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        fs::create_dir(&notes).unwrap();
        let a = notes.join("a.md");
        let b = notes.join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        type_into(&mut app, "A");
        app.request_open(b.clone());
        type_into(&mut app, "B");
        app.begin_rename(notes.clone());
        app.rename.as_mut().unwrap().name = "renamed".into();
        let mut time = 0.0;
        drive(
            &ctx,
            &mut app,
            &mut time,
            vec![shortcut(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        let renamed_a = dir.path().join("renamed/a.md");
        let renamed_b = dir.path().join("renamed/b.md");
        assert!(renamed_a.exists());
        assert!(renamed_b.exists());
        assert!(!a.exists());
        assert_eq!(app.tabs[0].doc.path(), Some(renamed_a.as_path()));
        assert_eq!(app.tabs[1].doc.path(), Some(renamed_b.as_path()));
        assert_eq!(tab_text(&app, 0), "a\nA");
        assert_eq!(tab_text(&app, 1), "b\nB");
        app.activate(0);
        app.save();
        app.activate(1);
        app.save();
        assert_eq!(fs::read_to_string(&renamed_a).unwrap(), "a\nA");
        assert_eq!(fs::read_to_string(&renamed_b).unwrap(), "b\nB");
    }

    #[test]
    fn a_clean_background_tab_reloads_and_a_dirty_one_is_marked() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        r.app.request_open(b.clone());
        r.settle();
        r.app.sidebar.visible = false;
        r.frame(vec![]);
        fs::write(&a, "from disk\n").unwrap();
        r.check_disk_now();
        assert_eq!(tab_text(&r.app, 0), "from disk\n");
        assert!(r.app.tabs[0].banner.is_none());
        assert!(r.app.tabs[0].disk.is_none());
        assert_eq!(text(&r.app), "b\n");
        assert!(r.app.tab().banner.is_none());

        type_into(&mut r.app, "mine");
        r.click_tab("a.md");
        assert_eq!(r.app.tab().doc.path(), Some(a.as_path()));
        fs::write(&b, "theirs\n").unwrap();
        r.check_disk_now();
        assert_eq!(tab_text(&r.app, 1), "b\nmine");
        assert!(matches!(r.app.tabs[1].disk, Some(Banner::DiskChanged)));
        assert!(r.app.tabs[1].banner.is_none());
        assert!(r.app.tab().banner.is_none());
        assert!(r.text_rect("b.md ● ↻").is_some());
        r.click_tab("b.md");
        assert!(matches!(r.app.tab().banner, Some(Banner::DiskChanged)));
        assert_eq!(text(&r.app), "b\nmine");
        r.app.sidebar.visible = true;
        r.frame(vec![]);
        let row = r.app.browser.row_rect(&b).expect("b.md");
        let marks = r.text_rects("↻");
        assert!(
            marks.iter().any(|mark| row.contains(mark.center())),
            "the sidebar marks the dirty file that changed on disk"
        );
    }

    #[test]
    fn a_background_reload_keeps_the_caret_and_scroll_line() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let body: String = (0..80).map(|i| format!("line {i} stays\n")).collect();
        fs::write(&a, &body).unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        let caret = r.app.tab().doc.line_to_byte(40);
        r.app.jump_to(caret);
        {
            let tab = r.app.tab_mut();
            let pos = inkmark_view::ScrollPos {
                line: 10,
                frac: 0.0,
            };
            tab.code.set_scroll_pos(pos);
            let Tab {
                doc, parse, live, ..
            } = tab;
            live.set_scroll_pos(doc, parse.output(), pos);
        }
        r.app.request_open(b.clone());
        r.settle();
        let mut next = body.clone();
        next.push_str("extra from disk\n");
        fs::write(&a, &next).unwrap();
        r.check_disk_now();
        assert_eq!(tab_text(&r.app, 0), next);
        r.click_tab("a.md");
        r.settle();
        assert_eq!(r.app.tab().code.selection().head, caret);
        assert_eq!(r.app.tab().live.selection().head, caret);
        assert_eq!(r.app.tab().code.scroll_pos().line, 10);
        assert_eq!(
            r.app
                .tab()
                .live
                .scroll_pos(&r.app.tab().doc, r.app.tab().parse.output())
                .line,
            10
        );
    }

    #[test]
    fn a_deleted_background_file_keeps_its_text_until_the_tab_is_selected() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        r.app.request_open(b.clone());
        r.settle();
        r.app.sidebar.visible = false;
        r.frame(vec![]);
        fs::remove_file(&a).unwrap();
        r.check_disk_now();
        assert_eq!(tab_text(&r.app, 0), "a\n");
        assert!(matches!(r.app.tabs[0].disk, Some(Banner::DiskMissing)));
        assert!(r.app.tabs[0].banner.is_none());
        assert!(r.app.tab().banner.is_none());
        assert!(r.text_rect("a.md ✕").is_some());
        r.click_tab("a.md");
        assert!(matches!(r.app.tab().banner, Some(Banner::DiskMissing)));
        assert_eq!(text(&r.app), "a\n");
    }

    #[test]
    fn forward_reopens_a_tab_that_was_closed() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "Intro.\n\nSee [the second part](b.md#second-part).\n").unwrap();
        fs::write(&b, "# First\n\ntext\n\n## Second part\n\nend\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        let clicked_from = text(&run.app).find("Intro").unwrap() + 2;
        run.app.jump_to(clicked_from);
        run.frame(vec![]);
        run.app.follow(text(&run.app).find("second part").unwrap());
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        let in_b = run.app.selection().head;
        run.key(egui::Key::ArrowLeft, egui::Modifiers::ALT);
        run.settle();
        assert_eq!(run.app.tab().doc.path(), Some(a.as_path()));
        run.app.sidebar.visible = false;
        run.frame(vec![]);
        run.click_text_button("b.md", egui::PointerButton::Middle);
        assert_eq!(run.app.tabs.len(), 1);
        run.key(egui::Key::ArrowRight, egui::Modifiers::ALT);
        run.settle();
        assert_eq!(run.app.tabs.len(), 2);
        assert_eq!(run.app.tab().doc.path(), Some(b.as_path()));
        assert_eq!(run.app.selection().head, in_b);
    }

    #[test]
    fn every_open_dirty_file_is_marked_in_the_sidebar() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut run.app, "a");
        run.app.request_open(b.clone());
        type_into(&mut run.app, "b");
        run.frame(vec![]);
        let a_row = run.app.browser.row_rect(&a).expect("a.md");
        let b_row = run.app.browser.row_rect(&b).expect("b.md");
        let marks = run.text_rects("●");
        assert!(
            marks.iter().any(|mark| a_row.contains(mark.center())),
            "a.md is dirty and not the file on screen"
        );
        assert!(
            marks.iter().any(|mark| b_row.contains(mark.center())),
            "b.md is dirty"
        );
    }

    #[test]
    fn the_close_prompt_stays_on_the_dirty_tab() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        type_into(&mut r.app, "unsaved");
        r.app.request_open(b.clone());
        r.frame(vec![]);
        let dirty = r.app.tabs[0].id;
        r.close_window();
        assert_eq!(r.app.confirm, Some(Confirm::Close(dirty)));
        assert_eq!(r.app.tab().doc.path(), Some(a.as_path()));
        r.frame(vec![]);
        assert!(r.text_rect("a.md has unsaved changes.").is_some());
        r.key(egui::Key::Tab, egui::Modifiers::COMMAND);
        assert_eq!(
            r.app.tab().doc.path(),
            Some(a.as_path()),
            "Ctrl+Tab does not switch under the prompt"
        );
        assert_eq!(r.app.confirm, Some(Confirm::Close(dirty)));
        r.click_text("Save");
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\nunsaved");
        assert_eq!(fs::read_to_string(&b).unwrap(), "b\n");
        assert!(r.sent(&ViewportCommand::Close));
    }

    #[test]
    fn save_as_writes_the_tab_that_opened_the_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = app(dir.path(), Some(a.clone()));
        type_into(&mut app, "A");
        app.request_open(b.clone());
        type_into(&mut app, "B");
        let id = app.tab().id;
        let (hook, kinds) = mpsc::channel();
        app.dialog_hook = Some(hook);
        app.spawn_dialog(DialogKind::SaveAs(id));
        assert_eq!(kinds.try_recv().unwrap(), DialogKind::SaveAs(id));
        app.activate(0);
        assert_eq!(app.tab().doc.path(), Some(a.as_path()));
        let dest = dir.path().join("out.md");
        let (tx, rx) = mpsc::channel();
        app.dialog = Some(rx);
        tx.send(DialogResult::SaveAs(id, Some(dest.clone())))
            .unwrap();
        app.poll_dialog(&ctx);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "b\nB");
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\n");
        assert_eq!(tab_text(&app, 0), "a\nA");
        let saved = app.tabs.iter().find(|tab| tab.id == id).unwrap();
        assert_eq!(saved.doc.path(), Some(dest.as_path()));
        assert!(!saved.doc.is_dirty());

        let gone = dir.path().join("gone.md");
        let (tx, rx) = mpsc::channel();
        app.dialog = Some(rx);
        tx.send(DialogResult::SaveAs(999, Some(gone.clone())))
            .unwrap();
        app.poll_dialog(&ctx);
        assert!(!gone.exists());
        assert!(app.after_save.is_none());
        assert_eq!(fs::read_to_string(&a).unwrap(), "a\n");
    }

    #[test]
    fn save_as_refuses_another_spelling_of_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let via = dir.path().join("sub/../draft.md");
        let direct = dir.path().join("draft.md");
        let nested = dir.path().join("sub/missing/../../draft.md");
        assert!(same_file(&via, &direct));
        assert!(same_file(&nested, &direct));
        assert!(!same_file(&dir.path().join("sub/draft.md"), &direct));

        let other = dir.path().join("other.md");
        fs::write(&other, "other\n").unwrap();
        let mut app = app(dir.path(), Some(via.clone()));
        assert!(!direct.exists());
        app.request_open(other);
        type_into(&mut app, "x");
        app.save_as(direct.clone());
        assert!(
            app.error.as_deref().unwrap().contains("already open"),
            "{:?}",
            app.error
        );
        assert!(!direct.exists());
        assert_eq!(tab_text(&app, 1), "other\nx");
        assert_eq!(app.tabs[0].doc.path(), Some(via.as_path()));
    }

    #[test]
    fn a_missing_background_file_is_not_reloaded_when_it_returns() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "a\n").unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        r.app.request_open(b.clone());
        r.settle();
        fs::remove_file(&a).unwrap();
        r.check_disk_now();
        assert_eq!(tab_text(&r.app, 0), "a\n");
        assert!(matches!(r.app.tabs[0].disk, Some(Banner::DiskMissing)));
        fs::write(&a, "replaced\n").unwrap();
        r.check_disk_now();
        assert_eq!(tab_text(&r.app, 0), "a\n");
        assert!(matches!(r.app.tabs[0].disk, Some(Banner::DiskChanged)));
        assert!(r.app.tabs[0].banner.is_none());
        r.click_tab("a.md");
        assert!(matches!(r.app.tab().banner, Some(Banner::DiskChanged)));
        assert_eq!(text(&r.app), "a\n");
    }

    #[test]
    fn a_reloaded_fold_is_kept_only_when_it_is_still_a_heading() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let src = "# One\nbody one\n# Two\nbody two\n";
        fs::write(&a, src).unwrap();
        fs::write(&b, "b\n").unwrap();
        let mut r = Run::new(dir.path(), Some(a.clone()));
        r.click_code_mark("▾");
        r.click_code_mark("▾");
        let before = r.app.tab().code.hidden_ranges().to_vec();
        assert_eq!(before.len(), 2, "{before:?}");
        r.app.request_open(b.clone());
        r.settle();
        let replaced = "plain\nbody one\n# Two\nbody two\n";
        assert_eq!(replaced.len(), src.len());
        fs::write(&a, replaced).unwrap();
        r.check_disk_now();
        assert_eq!(tab_text(&r.app, 0), replaced);
        assert!(
            r.app.tabs[0].code.hidden_ranges().is_empty(),
            "the body stays visible until the new parse settles"
        );
        r.click_tab("a.md");
        r.settle();
        let after = r.app.tab().code.hidden_ranges().to_vec();
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(after[0], before[1]);
    }

    #[test]
    fn the_empty_list_scrolls_the_selected_row_into_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for i in 0..10 {
            let path = dir.path().join(format!("n{i:02}.md"));
            fs::write(&path, "x\n").unwrap();
            paths.push(path);
        }
        let mut run = Run::new(dir.path(), Some(paths[0].clone()));
        for path in paths.iter().skip(1) {
            run.app.request_open(path.clone());
        }
        run.settle();
        run.app.sidebar.visible = false;
        while !run.app.tabs.is_empty() {
            run.key(egui::Key::W, egui::Modifiers::COMMAND);
        }
        assert!(run.app.tabs.is_empty());
        run.ctx
            .memory_mut(|m| m.surrender_focus(egui::Id::new("file_browser")));
        let size = egui::vec2(1000.0, 200.0);
        run.frame_sized(vec![], size);
        assert_eq!(run.app.empty_recent, 0);
        for _ in 0..9 {
            run.frame_sized(
                vec![shortcut(egui::Key::ArrowDown, egui::Modifiers::NONE)],
                size,
            );
        }
        run.frame_sized(vec![], size);
        assert_eq!(run.app.empty_recent, 9);
        assert!(
            run.text_in_view("n00.md"),
            "Down leaves the selected row on screen"
        );
        assert!(
            !run.text_in_view("n09.md"),
            "the first row scrolls off a short window"
        );
    }

    fn write_session(dir: &Path, active: usize, tabs: Vec<session_tabs::StoredTab>) {
        session_tabs::save(&dir.join("tabs"), &session_tabs::Session { active, tabs }).unwrap();
    }

    fn stored(
        path: &Path,
        preview: bool,
        caret: usize,
        line: usize,
        frac: f32,
    ) -> session_tabs::StoredTab {
        session_tabs::StoredTab {
            path: path.to_path_buf(),
            preview,
            caret,
            scroll_line: line,
            scroll_frac: frac,
        }
    }

    #[test]
    fn reopening_restores_tabs_the_caret_and_the_scroll() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "one\ntwo\nthree\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::with_recent(
            &ctx,
            Some(a.clone()),
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        app.open_tab(b.clone(), false);
        app.activate(0);
        let caret = app.tab().doc.line_to_byte(2);
        app.jump_to(caret);
        let scroll = ScrollPos { line: 1, frac: 0.5 };
        {
            let tab = app.tab_mut();
            tab.code.set_scroll_pos(scroll);
            let Tab {
                doc, parse, live, ..
            } = tab;
            live.set_scroll_pos(doc, parse.output(), scroll);
        }
        app.remember_session();

        let ctx = egui::Context::default();
        let app = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert_eq!(app.tabs.len(), 2);
        assert!(same_file(app.tabs[0].doc.path().unwrap(), &a));
        assert!(same_file(app.tabs[1].doc.path().unwrap(), &b));
        assert!(same_file(app.tab().doc.path().unwrap(), &a));
        assert!(!app.tabs[0].preview);
        assert!(app.tabs[1].preview);
        assert_eq!(app.tab().code.selection().head, caret);
        assert_eq!(app.tab().live.selection().head, caret);
        assert_eq!(app.tab().code.scroll_pos().line, 1);
        assert!((app.tab().code.scroll_pos().frac - 0.5).abs() < 0.001);
        assert_eq!(
            app.tab()
                .live
                .scroll_pos(&app.tab().doc, app.tab().parse.output())
                .line,
            1
        );
        assert!(app.error.is_none());
        assert!(app.recent_list.is_none());
    }

    #[test]
    fn a_missing_tab_is_skipped_and_a_bad_session_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let gone = dir.path().join("gone.md");
        fs::write(&a, "keep\n").unwrap();
        write_session(
            dir.path(),
            1,
            vec![stored(&a, false, 2, 0, 0.0), stored(&gone, true, 0, 0, 0.0)],
        );
        let ctx = egui::Context::default();
        let app = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert_eq!(app.tabs.len(), 1);
        assert!(same_file(app.tab().doc.path().unwrap(), &a));
        assert_eq!(app.tab().code.selection().head, 2);
        assert!(!app.tab().preview);
        assert!(app.error.is_none());
        assert!(!gone.exists());

        let bad = dir.path().join("bad-state");
        fs::create_dir(&bad).unwrap();
        let store = bad.join("tabs");
        fs::write(&store, "nope\n").unwrap();
        let ignored = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(bad.join("recent"))),
        );
        assert!(ignored.tab().doc.path().is_none());
        assert!(ignored.error.is_none());
        assert_eq!(fs::read_to_string(&store).unwrap(), "nope\n");
    }

    #[test]
    fn a_live_only_session_stays_live_only() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "alpha\n").unwrap();
        write_session(dir.path(), 0, vec![stored(&a, false, 0, 0, 0.0)]);
        let layout = dir.path().join("layout");
        fs::write(&layout, "live\n1\n1\n0.5\n200\n1\n").unwrap();
        let before = fs::read_to_string(&layout).unwrap();
        let ctx = egui::Context::default();
        let app = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert!(app.mode == Mode::Live);
        assert!(app.tab().focus == Pane::Live);
        assert!(same_file(app.tab().doc.path().unwrap(), &a));
        assert_eq!(fs::read_to_string(&layout).unwrap(), before);
    }

    #[test]
    fn an_unreadable_tab_is_kept_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.md");
        let good = dir.path().join("good.md");
        fs::write(&bad, [0xff]).unwrap();
        fs::write(&good, "keep\n").unwrap();
        write_session(
            dir.path(),
            0,
            vec![
                stored(&bad, false, 4, 1, 0.0),
                stored(&good, false, 2, 0, 0.0),
            ],
        );
        let ctx = egui::Context::default();
        let app = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert_eq!(app.tabs.len(), 1);
        assert!(same_file(app.tab().doc.path().unwrap(), &good));
        let error = app.error.expect("the unreadable file is reported");
        assert!(error.contains("bad.md"), "{error}");
        assert!(error.contains("UTF-8"), "{error}");
        let kept = session_tabs::load(&dir.path().join("tabs")).unwrap();
        assert!(
            kept.tabs.iter().any(|tab| same_file(&tab.path, &bad)),
            "the session dropped the unreadable file"
        );
        assert_eq!(
            kept.tabs
                .iter()
                .find(|tab| same_file(&tab.path, &bad))
                .unwrap()
                .caret,
            4
        );

        fs::write(&bad, "fixed\n").unwrap();
        let again = App::with_recent(
            &ctx,
            None,
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert!(again.error.is_none(), "{:?}", again.error);
        assert_eq!(again.tabs.len(), 2);
        assert!(
            again
                .tabs
                .iter()
                .any(|tab| tab.doc.path().is_some_and(|p| same_file(p, &bad)))
        );
        assert!(
            again
                .tabs
                .iter()
                .any(|tab| tab.doc.path().is_some_and(|p| same_file(p, &good)))
        );
        let fixed = again
            .tabs
            .iter()
            .find(|tab| tab.doc.path().is_some_and(|p| same_file(p, &bad)))
            .unwrap();
        assert_eq!(fixed.code.selection().head, 4);
    }

    #[test]
    fn a_file_argument_opens_beside_the_restored_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        fs::write(&c, "gamma\n").unwrap();
        write_session(
            dir.path(),
            0,
            vec![stored(&a, false, 3, 0, 0.0), stored(&b, false, 1, 0, 0.0)],
        );
        let ctx = egui::Context::default();
        let recent = recent::Recent::from_store(Some(dir.path().join("recent")));
        let opened = App::with_recent(&ctx, Some(c.clone()), recent);
        assert_eq!(opened.tabs.len(), 3);
        assert!(same_file(opened.tabs[0].doc.path().unwrap(), &a));
        assert!(same_file(opened.tabs[1].doc.path().unwrap(), &b));
        assert!(same_file(opened.tab().doc.path().unwrap(), &c));
        assert_eq!(opened.tabs[0].code.selection().head, 3);

        // The launch above saved the three open tabs. Put the two-tab
        // session back, so this launch is the one that already has the file.
        write_session(
            dir.path(),
            0,
            vec![stored(&a, false, 3, 0, 0.0), stored(&b, false, 1, 0, 0.0)],
        );
        let again = recent::Recent::from_store(Some(dir.path().join("recent")));
        let focused = App::with_recent(&ctx, Some(a.clone()), again);
        assert_eq!(focused.tabs.len(), 2);
        assert!(same_file(focused.tab().doc.path().unwrap(), &a));
        assert_eq!(focused.tabs[0].code.selection().head, 3);
    }

    #[test]
    fn a_folder_argument_does_not_restore_or_clear_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes");
        fs::create_dir(&notes).unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "alpha\n").unwrap();
        write_session(dir.path(), 0, vec![stored(&a, false, 1, 0, 0.0)]);
        let before = fs::read_to_string(dir.path().join("tabs")).unwrap();
        let ctx = egui::Context::default();
        let app = App::with_recent(
            &ctx,
            Some(notes.clone()),
            recent::Recent::from_store(Some(dir.path().join("recent"))),
        );
        assert!(app.tab().doc.path().is_none());
        assert_eq!(app.browser.root(), notes);
        assert_eq!(fs::read_to_string(dir.path().join("tabs")).unwrap(), before);
    }

    #[test]
    fn ctrl_shift_t_reopens_the_most_recently_closed_tab() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        let c = dir.path().join("c.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\nline\n").unwrap();
        fs::write(&c, "gamma\nline\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.request_open(b.clone());
        run.app.request_open(c.clone());
        let c_at = run.app.tab().doc.line_to_byte(1);
        run.app.jump_to(c_at);
        {
            let tab = run.app.tab_mut();
            let pos = ScrollPos {
                line: 1,
                frac: 0.25,
            };
            tab.code.set_scroll_pos(pos);
            let Tab {
                doc, parse, live, ..
            } = tab;
            live.set_scroll_pos(doc, parse.output(), pos);
        }
        run.key(egui::Key::W, egui::Modifiers::COMMAND);
        assert!(same_file(run.app.tab().doc.path().unwrap(), &b));
        let b_at = 2;
        run.app.jump_to(b_at);
        run.key(egui::Key::W, egui::Modifiers::COMMAND);
        assert!(same_file(run.app.tab().doc.path().unwrap(), &a));

        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::T, shift);
        assert!(same_file(run.app.tab().doc.path().unwrap(), &b));
        assert_eq!(run.app.tab().code.selection().head, b_at);
        assert!(!run.app.tab().preview);
        run.key(egui::Key::T, shift);
        assert!(same_file(run.app.tab().doc.path().unwrap(), &c));
        assert_eq!(run.app.tab().code.selection().head, c_at);
        assert_eq!(run.app.tab().code.scroll_pos().line, 1);
        assert_eq!(run.app.tabs.len(), 3);

        while !run.app.tabs.is_empty() {
            run.key(egui::Key::W, egui::Modifiers::COMMAND);
        }
        run.key(egui::Key::T, shift);
        assert_eq!(run.app.tabs.len(), 1);
        assert!(same_file(run.app.tab().doc.path().unwrap(), &a));
    }

    #[test]
    fn ctrl_shift_t_skips_a_deleted_file_and_keeps_ten() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.app.request_open(b.clone());
        run.key(egui::Key::W, egui::Modifiers::COMMAND);
        fs::remove_file(&b).unwrap();
        let shift = egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT);
        run.key(egui::Key::T, shift);
        assert_eq!(run.app.tabs.len(), 1);
        assert!(same_file(run.app.tab().doc.path().unwrap(), &a));
        assert!(!b.exists());

        let mut paths = Vec::new();
        for i in 0..11 {
            let path = dir.path().join(format!("n{i}.md"));
            fs::write(&path, format!("{i}\n")).unwrap();
            run.app.request_open(path.clone());
            paths.push(path);
        }
        while !run.app.tabs.is_empty() {
            run.key(egui::Key::W, egui::Modifiers::COMMAND);
        }
        // Ctrl+W closes the tab on screen and lands on the previous one, so
        // the files close from n10 down through n0 and then a. The stack
        // keeps the last ten of those. n9 and n10 were closed first.
        let mut expect = vec![a];
        expect.extend(paths.iter().take(9).cloned());
        for path in &expect {
            run.key(egui::Key::T, shift);
            let got = run.app.tab().doc.path().unwrap();
            assert!(
                same_file(got, path),
                "reopened {} wanted {}",
                got.display(),
                path.display()
            );
        }
        let open = run.app.tabs.len();
        assert_eq!(open, expect.len());
        run.key(egui::Key::T, shift);
        assert_eq!(run.app.tabs.len(), open);
    }

    #[test]
    fn fuzzed_tab_switches_keep_each_documents_keystrokes() {
        let iters: usize = std::env::var("FUZZ_ITERS")
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(40);
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "").unwrap();
        fs::write(&b, "").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.request_open(b);
        run.settle();
        run.app.sidebar.visible = false;
        run.frame(vec![]);
        let mut expected = [String::new(), String::new()];
        let mut state: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..iters {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            match state % 7 {
                0 => {
                    let before = run.app.active;
                    run.key(egui::Key::Tab, egui::Modifiers::COMMAND);
                    assert_ne!(run.app.active, before);
                }
                1 => {
                    let before = run.app.active;
                    run.key(
                        egui::Key::Tab,
                        egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
                    );
                    assert_ne!(run.app.active, before);
                }
                2 | 3 => {
                    let name = if run.app.active == 0 { "b.md" } else { "a.md" };
                    run.click_tab(name);
                    let want = usize::from(name == "b.md");
                    assert_eq!(run.app.active, want, "click on {name}");
                    assert!(run.app.tab().code.has_focus(&run.ctx));
                }
                _ => {
                    let ch = (b'a' + (state % 26) as u8) as char;
                    run.frame(vec![egui::Event::Text(ch.to_string())]);
                    expected[run.app.active].push(ch);
                }
            }
        }
        assert_eq!(tab_text(&run.app, 0), expected[0]);
        assert_eq!(tab_text(&run.app, 1), expected[1]);
    }

    #[test]
    fn a_background_tab_keeps_its_line_cache_until_the_budget_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "# A\n\nAlpha paragraph.\n").unwrap();
        fs::write(&b, "# B\n\nBeta paragraph.\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a.clone()));
        run.frame(vec![]);
        run.app.open_tab(b.clone(), true);
        run.frame(vec![]);
        assert!(run.app.tabs[1].code.cached_line_bytes() > 0);
        run.app.trim_background_line_caches_over(0);
        assert_eq!(run.app.tabs[0].code.cached_line_bytes(), 0);
        assert_eq!(run.app.tabs[0].live.cached_line_bytes(), 0);
        assert!(run.app.tabs[1].code.cached_line_bytes() > 0);
        // Under the real budget the background cache stays, so a switch
        // paints lines that were already shaped.
        run.app.open_tab(a, true);
        run.frame(vec![]);
        let kept = run.app.tabs[1].code.cached_line_bytes();
        run.app.trim_background_line_caches();
        assert_eq!(run.app.tabs[1].code.cached_line_bytes(), kept);
    }

    #[test]
    fn a_background_parse_does_not_ask_for_an_immediate_frame() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        fs::write(&a, "alpha\n").unwrap();
        fs::write(&b, "beta\n").unwrap();
        let mut run = Run::new(dir.path(), Some(a));
        run.app.open_tab(b, true);
        run.settle();
        run.frame(vec![]);
        let delay = run
            .out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|viewport| viewport.repaint_delay)
            .unwrap_or(std::time::Duration::MAX);
        assert!(
            delay > std::time::Duration::from_millis(100),
            "a background tab asked for a frame now ({delay:?})"
        );
    }
}
