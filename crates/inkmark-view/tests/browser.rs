//! Drives the file sidebar with synthetic egui events. Listings arrive from
//! the worker thread, so waits poll until a deadline instead of sleeping a
//! fixed time.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, pos2};
use inkmark_view::{BrowserOutput, FileBrowser};

const SCREEN: Rect = Rect::from_min_max(Pos2::ZERO, pos2(800.0, 600.0));

struct Harness {
    ctx: egui::Context,
    browser: FileBrowser,
    time: f64,
}

impl Harness {
    fn new(root: &Path) -> Self {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::Theme::Dark);
        let mut harness = Self {
            ctx,
            browser: FileBrowser::new(root),
            time: 0.0,
        };
        harness.frame(vec![]);
        harness
    }

    fn frame(&mut self, events: Vec<Event>) -> BrowserOutput {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(SCREEN),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        let browser = &mut self.browser;
        let mut output = BrowserOutput::default();
        let mut out = self.ctx.run_ui(input, |ui| {
            output = browser.show(ui);
        });
        out.textures_delta.clear();
        output
    }

    fn press(&mut self, key: Key) -> BrowserOutput {
        self.frame(vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }])
    }

    fn click(&mut self, pos: Pos2) -> BrowserOutput {
        let button = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        self.frame(vec![Event::PointerMoved(pos), button(true)]);
        self.frame(vec![button(false)])
    }

    fn wait_until(&mut self, mut pred: impl FnMut(&mut FileBrowser) -> bool) {
        let start = Instant::now();
        loop {
            self.frame(vec![]);
            if pred(&mut self.browser) {
                return;
            }
            if start.elapsed() > Duration::from_secs(2) {
                panic!(
                    "sidebar did not update in time, rows: {:?}",
                    self.browser.row_names()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn keyboard_expands_collapses_moves_and_opens() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("a.md"), "a\n").unwrap();
    fs::write(dir.path().join("b.md"), "b\n").unwrap();
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_names() == ["sub", "b.md"]);

    h.browser.request_focus();
    h.press(Key::ArrowDown);
    h.press(Key::ArrowRight);
    h.wait_until(|browser| browser.row_names() == ["sub", "a.md", "b.md"]);

    // Left on an expanded folder collapses it.
    h.press(Key::ArrowLeft);
    h.frame(vec![]);
    assert_eq!(h.browser.row_names(), ["sub", "b.md"]);

    // Down onto the file, then Enter opens it.
    let output = h.press(Key::Enter);
    // Enter still lands on `sub` (a folder): it expands again.
    assert!(output.open_file.is_none());
    h.wait_until(|browser| browser.row_names().len() == 3);
    h.press(Key::ArrowDown);
    let output = h.press(Key::Enter);
    assert_eq!(output.open_file, Some(sub.join("a.md")));
}

#[test]
fn clicking_a_file_opens_it_and_a_folder_expands() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("a.md"), "a\n").unwrap();
    fs::write(dir.path().join("b.md"), "b\n").unwrap();
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_rect(&dir.path().join("b.md")).is_some());

    let file = h.browser.row_rect(&dir.path().join("b.md")).unwrap();
    let output = h.click(file.center());
    assert_eq!(output.open_file, Some(dir.path().join("b.md")));

    let folder = h.browser.row_rect(&sub).unwrap();
    let output = h.click(folder.center());
    assert!(output.open_file.is_none());
    h.wait_until(|browser| browser.row_names().contains(&"a.md".to_string()));
    let folder = h.browser.row_rect(&sub).unwrap();
    h.click(folder.center());
    h.frame(vec![]);
    assert!(!h.browser.row_names().contains(&"a.md".to_string()));
}

#[test]
fn up_open_folder_and_refresh_are_in_the_header() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("note.md"), "n\n").unwrap();
    let mut h = Harness::new(&nested);
    h.wait_until(|browser| browser.row_names() == ["note.md"]);

    let open = h.browser.open_folder_rect().unwrap();
    let output = h.click(open.center());
    assert!(output.open_folder);

    let up = h.browser.up_rect().unwrap();
    h.click(up.center());
    assert_eq!(h.browser.root(), dir.path());

    h.wait_until(|browser| browser.row_names().iter().any(|name| name == "nested"));
    fs::write(dir.path().join("later.md"), "l\n").unwrap();
    let refresh = h.browser.refresh_rect().expect("refresh button");
    h.click(refresh.center());
    h.wait_until(|browser| browser.row_names().iter().any(|name| name == "later.md"));
}

#[test]
fn only_visible_rows_are_painted() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..400 {
        fs::write(dir.path().join(format!("note{i}.md")), "").unwrap();
    }
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_count() == 400);
    let painted = h.browser.painted();
    assert!((1..40).contains(&painted), "painted {painted} rows of 400");
}

fn wait_for_names(h: &mut Harness, mut pred: impl FnMut(&[String]) -> bool) {
    let start = Instant::now();
    loop {
        h.frame(vec![]);
        let names = h.browser.row_names();
        if pred(&names) {
            return;
        }
        if start.elapsed() > Duration::from_secs(1) {
            panic!("sidebar did not update within 1s, rows: {names:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_new_sibling_does_not_collapse_an_expanded_folder() {
    let dir = tempfile::tempdir().unwrap();
    let projects = dir.path().join("projects");
    fs::create_dir(&projects).unwrap();
    fs::write(projects.join("note.md"), "n\n").unwrap();
    fs::write(dir.path().join("todo.md"), "t\n").unwrap();
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_names().iter().any(|name| name == "projects"));
    h.browser.request_focus();
    h.press(Key::ArrowDown);
    h.press(Key::ArrowRight);
    h.wait_until(|browser| browser.row_names().iter().any(|name| name == "note.md"));

    fs::write(dir.path().join("later.md"), "l\n").unwrap();
    wait_for_names(&mut h, |names| names.iter().any(|name| name == "later.md"));
    assert!(
        h.browser.row_names().iter().any(|name| name == "note.md"),
        "expanded folder collapsed: {:?}",
        h.browser.row_names()
    );
}

#[test]
fn external_create_rename_and_delete_show_up_in_the_sidebar() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "a\n").unwrap();
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_names() == ["a.md"]);

    fs::write(dir.path().join("b.md"), "b\n").unwrap();
    wait_for_names(&mut h, |names| names.iter().any(|name| name == "b.md"));

    fs::rename(dir.path().join("b.md"), dir.path().join("c.md")).unwrap();
    wait_for_names(&mut h, |names| {
        names.iter().any(|name| name == "c.md") && !names.iter().any(|name| name == "b.md")
    });

    fs::remove_file(dir.path().join("c.md")).unwrap();
    wait_for_names(&mut h, |names| !names.iter().any(|name| name == "c.md"));
}

#[test]
#[ignore]
fn bench_sidebar_10k_folder() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..10_000 {
        fs::write(dir.path().join(format!("file{i}.md")), "").unwrap();
    }
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_count() == 10_000);
    let mut samples = Vec::new();
    for _ in 0..60 {
        let start = Instant::now();
        h.frame(vec![]);
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    samples.sort_by(f64::total_cmp);
    let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "10k sidebar frame ms p50 {:.2} p95 {:.2} max {:.2}, painted {}",
        at(0.5),
        at(0.95),
        at(1.0),
        h.browser.painted()
    );
    assert!(at(0.95) < 16.0, "p95 {:.2} ms", at(0.95));
    assert!(h.browser.painted() < 40);
}
