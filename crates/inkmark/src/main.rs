use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, Key, KeyboardShortcut, Modifiers, Rect, RichText, Stroke, UiBuilder,
    ViewportCommand, pos2,
};
use inkmark_buffer::{DiskStatus, Document, LineEnding, OpenError};
use inkmark_parse::{ParseState, PulldownParser};
use inkmark_text::Fonts;
use inkmark_view::{CodeView, LiveView};

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

mod measure;
mod recent;

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

enum Banner {
    Error(String),
    /// Another program changed the file since we loaded or saved it.
    DiskChanged,
    DiskMissing,
}

/// An action waiting on "discard unsaved changes?".
#[derive(Clone, PartialEq, Eq)]
enum Confirm {
    /// Ctrl+O: show the open dialog.
    Open,
    /// Open this file (from the recent list).
    OpenPath(PathBuf),
    Close,
}

enum DialogResult {
    Open(Option<PathBuf>),
    SaveAs(Option<PathBuf>),
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
    /// A dialog took keyboard focus from the panes last frame.
    modal_was_open: bool,
}

impl App {
    fn new(ctx: &egui::Context, path: Option<PathBuf>) -> Self {
        // One font database and glyph atlas for both panes.
        let fonts = Fonts::shared(ctx);
        let mut app = Self {
            doc: Document::default(),
            code: CodeView::with_fonts(fonts.clone(), egui::Id::new("code_view")),
            live: LiveView::with_fonts(fonts, egui::Id::new("live_view")),
            mode: Mode::Split,
            focus: Pane::Code,
            parse: {
                let ctx = ctx.clone();
                // The swap point for a GFM parser later.
                ParseState::new(Arc::new(PulldownParser), &Document::default(), move || {
                    ctx.request_repaint()
                })
            },
            banner: None,
            dialog: None,
            confirm: None,
            close_after_save: false,
            close_allowed: false,
            next_disk_check: Instant::now() + DISK_CHECK_INTERVAL,
            title: String::new(),
            measure: None,
            recent: recent::Recent::load(),
            recent_list: None,
            modal_was_open: false,
        };
        match path {
            Some(path) => app.open(path),
            // Nothing to open: offer the recent files.
            None if !app.recent.entries().is_empty() => app.recent_list = Some(0),
            None => {}
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
        self.recent.add(&path);
        match Document::open(&path) {
            Ok(doc) => self.doc = doc,
            // A path that doesn't exist yet becomes a new file there.
            Err(OpenError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                self.doc = Document::default();
                self.doc.set_path(path);
            }
            Err(e) => {
                self.banner = Some(Banner::Error(format!(
                    "Couldn't open {}: {e}",
                    path.display()
                )));
                return;
            }
        }
        self.code.reset();
        self.live.reset();
        self.parse.reset(&self.doc);
        self.banner = None;
    }

    fn reload(&mut self) {
        if let Some(path) = self.doc.path().map(|p| p.to_path_buf()) {
            self.open(path);
        }
    }

    fn save(&mut self) {
        if self.doc.path().is_none() {
            return self.spawn_dialog(false);
        }
        // Never overwrite someone else's changes without asking.
        if matches!(self.doc.disk_status(), Ok(DiskStatus::Modified)) {
            self.banner = Some(Banner::DiskChanged);
            return;
        }
        match self.doc.save() {
            Ok(()) => self.saved(),
            Err(e) => self.banner = Some(Banner::Error(format!("Couldn't save: {e}"))),
        }
    }

    fn save_as(&mut self, path: PathBuf) {
        match self.doc.save_as(&path) {
            Ok(()) => {
                self.recent.add(&path);
                self.saved();
            }
            Err(e) => {
                self.banner = Some(Banner::Error(format!(
                    "Couldn't save {}: {e}",
                    path.display()
                )))
            }
        }
    }

    fn saved(&mut self) {
        if matches!(self.banner, Some(Banner::DiskChanged | Banner::DiskMissing)) {
            self.banner = None;
        }
        if self.close_after_save {
            self.close_allowed = true;
        }
    }

    /// Runs the portal file dialog on a thread so the UI keeps drawing.
    fn spawn_dialog(&mut self, open: bool) {
        if self.dialog.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let dir = self
            .doc
            .path()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf());
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new().add_filter("Markdown", MARKDOWN_EXTENSIONS);
            if let Some(dir) = dir {
                dialog = dialog.set_directory(dir);
            }
            let result = if open {
                DialogResult::Open(dialog.pick_file())
            } else {
                DialogResult::SaveAs(dialog.set_file_name("untitled.md").save_file())
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
            Ok(DialogResult::Open(Some(path))) => self.open(path),
            Ok(DialogResult::SaveAs(Some(path))) => self.save_as(path),
            Ok(DialogResult::Open(None) | DialogResult::SaveAs(None)) => {
                self.close_after_save = false;
            }
            Err(TryRecvError::Disconnected) => {
                self.banner = Some(Banner::Error(
                    "The file dialog failed. Is xdg-desktop-portal running?".into(),
                ));
            }
        }
        self.dialog = None;
    }

    fn check_disk(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if now >= self.next_disk_check {
            self.next_disk_check = now + DISK_CHECK_INTERVAL;
            if !matches!(self.banner, Some(Banner::Error(_))) {
                self.banner = match self.doc.disk_status() {
                    Ok(DiskStatus::Modified) => Some(Banner::DiskChanged),
                    Ok(DiskStatus::Missing) => Some(Banner::DiskMissing),
                    _ => None,
                };
            }
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
                self.mode = Mode::Split;
                self.focus_pane(ctx, Pane::Live);
            }
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (cycle, code, live, minimap, recent) = ctx.input_mut(|i| {
            // egui ignores extra Alt when matching; Ctrl+Alt+digit sets headings.
            let alt = i.modifiers.alt;
            (
                i.consume_shortcut(&CYCLE_MODE),
                !alt && i.consume_shortcut(&FOCUS_CODE),
                !alt && i.consume_shortcut(&FOCUS_LIVE),
                i.consume_shortcut(&TOGGLE_MINIMAP),
                i.consume_shortcut(&RECENT),
            )
        });
        if recent {
            self.recent_list = match self.recent_list {
                Some(_) => None,
                None => Some(0),
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
        let (save_as, save, open) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&SAVE_AS),
                i.consume_shortcut(&SAVE),
                i.consume_shortcut(&OPEN),
            )
        });
        if save_as {
            self.spawn_dialog(false);
        } else if save {
            self.save();
        }
        if open {
            if self.doc.is_dirty() {
                self.confirm = Some(Confirm::Open);
            } else {
                self.spawn_dialog(true);
            }
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
        let Some(banner) = &self.banner else { return };
        let mut action = None;
        ui.horizontal(|ui| match banner {
            Banner::Error(message) => {
                ui.label(RichText::new(message).color(Color32::from_rgb(255, 140, 120)));
                if ui.button("Dismiss").clicked() {
                    action = Some("dismiss");
                }
            }
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
            Some("dismiss") => self.banner = None,
            Some("reload") => self.reload(),
            Some("keep") => {
                self.banner = match self.doc.acknowledge_disk_state() {
                    Ok(()) => None,
                    Err(e) => Some(Banner::Error(format!("Couldn't check the file: {e}"))),
                };
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
        let open = self.recent_list.is_some() || self.confirm.is_some();
        if open {
            self.code.release_focus(ctx);
            self.live.release_focus(ctx);
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
                    self.spawn_dialog(true);
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
            ("discard", Confirm::Open) => self.spawn_dialog(true),
            _ => {}
        }
    }
}

impl App {
    fn panes(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
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

        if self.banner.is_some() {
            egui::Panel::top("banner").show(ui, |ui| self.banner_ui(ui));
        }
        egui::Panel::bottom("status").show(ui, |ui| self.status_ui(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| self.panes(ui));
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
