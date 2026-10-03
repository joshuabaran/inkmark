//! The desktop's colors. Omarchy's current theme (`colors.toml`) when there
//! is one, reloaded when `omarchy theme set` switches it; otherwise inkmark's
//! built-in dark or light theme, following the desktop's light/dark setting.
//! `INKMARK_THEME=<colors.toml>` picks a palette file instead (for trying a
//! theme, and for screenshots).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32};
use inkmark_view::theme::{self, Palette, Theme};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// A theme switch rewrites several files; wait for it to finish.
const SETTLE: Duration = Duration::from_millis(150);

pub struct OsTheme {
    /// An explicit palette file (INKMARK_THEME); otherwise Omarchy's, if any.
    fixed_file: Option<PathBuf>,
    omarchy: Option<PathBuf>,
    /// Keeps the watch on Omarchy's `current` folder alive.
    _watcher: Option<RecommendedWatcher>,
    changes: Option<Receiver<()>>,
    /// The last change seen, until things settle.
    changed_at: Option<Instant>,
    /// The desktop light/dark setting the built-in theme was chosen for.
    system: Option<Option<egui::Theme>>,
    following: bool,
}

impl OsTheme {
    /// Follows the desktop: applies the current theme to `ctx` now, and
    /// again whenever it changes (see [`poll`](Self::poll)).
    pub fn follow(ctx: &egui::Context) -> Self {
        let fixed_file = std::env::var_os("INKMARK_THEME").map(PathBuf::from);
        let omarchy = omarchy_colors();
        let (watcher, changes) = match (&fixed_file, &omarchy) {
            (None, Some(colors)) => watch(colors, ctx).unzip(),
            _ => (None, None),
        };
        let mut os = Self {
            fixed_file,
            omarchy,
            _watcher: watcher,
            changes,
            changed_at: None,
            system: None,
            following: true,
        };
        os.apply(ctx);
        os
    }

    /// Doesn't follow anything (tests): the context keeps its theme.
    pub fn fixed() -> Self {
        Self {
            fixed_file: None,
            omarchy: None,
            _watcher: None,
            changes: None,
            changed_at: None,
            system: None,
            following: false,
        }
    }

    /// Call every frame: picks up a theme switch or a light/dark change.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if !self.following {
            return;
        }
        if let Some(rx) = &self.changes {
            while rx.try_recv().is_ok() {
                self.changed_at = Some(Instant::now());
            }
        }
        if let Some(at) = self.changed_at {
            // One sample. Sampling twice can cross the deadline between the
            // check and the subtraction, and `Duration` panics on that.
            if let Some(left) = time_left(at, SETTLE) {
                ctx.request_repaint_after(left);
                return;
            }
            self.changed_at = None;
            self.apply(ctx);
            return;
        }
        // Using the built-in themes: follow the desktop's light/dark switch.
        if let Some(seen) = self.system
            && ctx.system_theme() != seen
        {
            self.apply(ctx);
        }
    }

    fn apply(&mut self, ctx: &egui::Context) {
        let file = self
            .fixed_file
            .clone()
            .or_else(|| self.omarchy.clone().filter(|p| p.exists()));
        if let Some(path) = file {
            match load_palette(&path) {
                Ok(palette) => {
                    let theme = Theme::from_palette(&palette);
                    let dark = theme.dark;
                    theme::set(ctx, theme);
                    self.system = None;
                    // One line per apply, including a switch after the settle.
                    crate::session::line(&format!("theme dark={dark}"));
                    return;
                }
                Err(e) => eprintln!("inkmark: {}: {e}", path.display()),
            }
        }
        let system = ctx.system_theme();
        self.system = Some(system);
        let theme = match system {
            Some(egui::Theme::Light) => Theme::light(),
            _ => Theme::dark(),
        };
        theme::set(ctx, theme);
    }
}

/// Time left before `window` elapses, or nothing once it has.
/// A single clock read, so the caller never subtracts past zero.
pub fn time_left(started: Instant, window: Duration) -> Option<Duration> {
    window
        .checked_sub(started.elapsed())
        .filter(|left| !left.is_zero())
}

/// Where Omarchy keeps the current theme's palette.
pub fn omarchy_colors() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("omarchy/current/theme/colors.toml"))
}

/// Watches `current/` (the theme folder is replaced as a whole on a switch,
/// so watching it directly would lose the watch). Wakes the UI on a change.
fn watch(colors: &Path, ctx: &egui::Context) -> Option<(RecommendedWatcher, Receiver<()>)> {
    let current = colors.parent()?.parent()?;
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        // `notify` reports the open that happens when the palette is read.
        // Treating that as a change reads the file again and never settles.
        let Ok(event) = event else {
            return;
        };
        if !palette_changed(event.kind) {
            return;
        }
        let _ = tx.send(());
        ctx.request_repaint();
    })
    .ok()?;
    watcher.watch(current, RecursiveMode::Recursive).ok()?;
    Some((watcher, rx))
}

/// A real palette change. Opens and reads are not: those fire while loading.
fn palette_changed(kind: notify::EventKind) -> bool {
    matches!(
        kind,
        notify::EventKind::Modify(_)
            | notify::EventKind::Create(_)
            | notify::EventKind::Remove(_)
            | notify::EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write,
            ))
    )
}

/// Reads an Omarchy `colors.toml`: `mode = "dark" | "light"` and colors as
/// `"#rrggbb"`. Unknown keys are ignored; missing colors are left to the
/// mapping's fallbacks.
pub fn load_palette(path: &Path) -> Result<Palette, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    parse_palette(&text)
}

pub fn parse_palette(text: &str) -> Result<Palette, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let color = |key: &str| -> Result<Option<Color32>, String> {
        match table.get(key) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Color32::from_hex(s.trim())
                .map(Some)
                .map_err(|_| format!("{key} = {s:?} isn't a #rrggbb color")),
            Some(_) => Err(format!("{key} isn't a \"#rrggbb\" string")),
        }
    };
    let dark = match table.get("mode").and_then(|m| m.as_str()) {
        Some("light") => false,
        Some("dark") | None => {
            // No mode: judge by the background.
            color("background")?.is_none_or(|bg| theme::contrast(bg, Color32::BLACK) < 7.0)
        }
        Some(other) => return Err(format!("mode = {other:?} isn't dark or light")),
    };
    Ok(Palette {
        dark,
        background: color("background")?,
        foreground: color("foreground")?,
        bright_foreground: color("bright_foreground")?,
        accent: color("accent")?,
        selection: color("selection")?,
        muted: color("muted")?,
        red: color("red")?,
        green: color("green")?,
        yellow: color("yellow")?,
        orange: color("orange")?,
        blue: color("blue")?,
        cyan: color("cyan")?,
        magenta: color("magenta")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_parse() {
        let p = parse_palette(
            "mode = \"light\"\naccent = \"#205EA6\"\nbackground = \"#FFFCF0\"\nforeground = \"#100F0F\"\nunknown = 3\n",
        )
        .unwrap();
        assert!(!p.dark);
        assert_eq!(p.accent, Color32::from_hex("#205ea6").ok());
        assert_eq!(p.green, None);
        let p = parse_palette("background = \"#060B1E\"\n").unwrap();
        assert!(p.dark, "a dark background without a mode is dark");
        assert!(
            parse_palette("accent = \"blue\"")
                .unwrap_err()
                .contains("accent")
        );
        assert!(parse_palette("mode = \"sepia\"").is_err());
        assert!(parse_palette("not toml").is_err());
    }

    #[test]
    fn every_installed_omarchy_theme_is_readable() {
        // Runs where Omarchy is installed (skipped in CI): each bundled
        // palette must map to a theme that passes the contrast checks.
        let Ok(dirs) = std::fs::read_dir("/usr/share/omarchy/themes") else {
            return;
        };
        let mut checked = 0;
        for dir in dirs.flatten() {
            let path = dir.path().join("colors.toml");
            let Ok(palette) = load_palette(&path) else {
                continue;
            };
            let t = Theme::from_palette(&palette);
            for (name, c, min) in [
                ("text", t.text, 4.5),
                ("heading", t.heading, 4.5),
                ("code", t.code, 4.5),
                ("link", t.link, 4.5),
                ("markup", t.markup, 3.0),
            ] {
                let ratio = theme::contrast(c, t.background);
                assert!(ratio >= min, "{}: {name} {ratio:.2}", path.display());
            }
            checked += 1;
        }
        eprintln!("checked {checked} Omarchy palettes");
    }

    #[test]
    fn an_open_or_a_read_is_not_a_palette_change() {
        use notify::EventKind;
        use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};

        assert!(!palette_changed(EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(!palette_changed(EventKind::Access(AccessKind::Read)));
        assert!(!palette_changed(EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(palette_changed(EventKind::Modify(ModifyKind::Any)));
        assert!(palette_changed(EventKind::Create(CreateKind::File)));
        assert!(palette_changed(EventKind::Remove(RemoveKind::File)));
        assert!(palette_changed(EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
    }

    #[test]
    fn a_deadline_that_has_passed_has_no_time_left() {
        let started = Instant::now().checked_sub(Duration::from_secs(5)).unwrap();
        assert_eq!(time_left(started, Duration::from_secs(1)), None);
        assert_eq!(time_left(Instant::now(), Duration::ZERO), None);
        assert!(time_left(Instant::now(), Duration::from_secs(30)).is_some());
    }

    #[test]
    fn a_switch_is_picked_up_after_it_settles() {
        let dir = tempfile::tempdir().unwrap();
        let colors = dir.path().join("omarchy/current/theme/colors.toml");
        std::fs::create_dir_all(colors.parent().unwrap()).unwrap();
        std::fs::write(
            &colors,
            "mode = \"dark\"\nbackground = \"#101010\"\nforeground = \"#e0e0e0\"\n",
        )
        .unwrap();
        let ctx = egui::Context::default();
        let (watcher, changes) = watch(&colors, &ctx).unzip();
        let mut os = OsTheme {
            fixed_file: None,
            omarchy: Some(colors.clone()),
            _watcher: watcher,
            changes,
            changed_at: None,
            system: None,
            following: true,
        };
        os.apply(&ctx);
        assert_eq!(
            theme::current(&ctx).background,
            Color32::from_hex("#101010").unwrap()
        );
        // `omarchy theme set` replaces the folder.
        let theme_dir = colors.parent().unwrap();
        std::fs::remove_dir_all(theme_dir).unwrap();
        std::fs::create_dir_all(theme_dir).unwrap();
        std::fs::write(
            &colors,
            "mode = \"light\"\nbackground = \"#fafafa\"\nforeground = \"#202020\"\n",
        )
        .unwrap();
        let start = Instant::now();
        while theme::current(&ctx).dark {
            os.poll(&ctx);
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "the switch never applied"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            theme::current(&ctx).background,
            Color32::from_hex("#fafafa").unwrap()
        );
    }
}
