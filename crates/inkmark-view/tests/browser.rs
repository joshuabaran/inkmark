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
    /// What the last frame drew, to find menu items by their text.
    shapes: Vec<egui::epaint::ClippedShape>,
}

impl Harness {
    fn new(root: &Path) -> Self {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::Theme::Dark);
        let mut harness = Self {
            ctx,
            browser: FileBrowser::new(root),
            time: 0.0,
            shapes: Vec::new(),
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
        self.shapes = out.shapes;
        output
    }

    /// Where the last frame drew `text`, e.g. a context-menu item.
    fn text_rect(&self, text: &str) -> Option<Rect> {
        fn find(shape: &egui::Shape, text: &str) -> Option<Rect> {
            match shape {
                egui::Shape::Text(t) if t.galley.text() == text => {
                    Some(t.galley.rect.translate(t.pos.to_vec2()))
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| find(s, text)),
                _ => None,
            }
        }
        self.shapes.iter().find_map(|c| find(&c.shape, text))
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
fn keyboard_scroll_keeps_the_selected_row_in_view() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..80 {
        fs::write(dir.path().join(format!("note{i:02}.md")), "").unwrap();
    }
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_count() == 80);
    let names = h.browser.row_names();
    h.browser.request_focus();
    for _ in 0..40 {
        h.press(Key::ArrowDown);
    }
    let selected = dir.path().join(&names[39]);
    let before = h
        .browser
        .row_rect(&selected)
        .expect("the selected row should be on screen");
    assert!(
        h.browser.row_rect(&dir.path().join(&names[0])).is_none(),
        "the list never left the top"
    );

    let next = dir.path().join(&names[40]);
    h.press(Key::ArrowDown);
    let after = h
        .browser
        .row_rect(&next)
        .expect("the next row should be on screen");
    assert!(
        (after.top() - before.top()).abs() < 40.0,
        "one row down jumped from {} to {}",
        before.top(),
        after.top()
    );

    h.browser.set_current(Some(dir.path().join(&names[0])));
    h.frame(vec![]);
    assert!(
        h.browser.row_rect(&dir.path().join(&names[0])).is_some(),
        "a file above the window was not scrolled back into view"
    );
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

impl Harness {
    /// Press on `from`, move to `to` in steps, release there. Returns the
    /// outputs of every frame, so a drop on any of them is seen.
    fn drag(&mut self, from: Pos2, to: Pos2) -> Vec<BrowserOutput> {
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        let mut outputs = vec![self.frame(vec![Event::PointerMoved(from), button(from, true)])];
        for i in 1..=10 {
            let pos = from + (to - from) * (i as f32 / 10.0);
            outputs.push(self.frame(vec![Event::PointerMoved(pos)]));
        }
        outputs.push(self.frame(vec![button(to, false)]));
        outputs.push(self.frame(vec![]));
        outputs
    }
}

fn dropped(outputs: Vec<BrowserOutput>) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    outputs.into_iter().find_map(|o| o.dropped)
}

#[test]
fn f2_and_delete_ask_about_the_selected_row() {
    let dir = tempfile::tempdir().unwrap();
    let b = dir.path().join("b.md");
    fs::write(&b, "b\n").unwrap();
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_rect(&b).is_some());
    h.click(h.browser.row_rect(&b).unwrap().center());
    let output = h.press(Key::F2);
    assert_eq!(output.rename, Some(b.clone()));
    assert!(output.trash.is_none());
    let output = h.press(Key::Delete);
    assert_eq!(output.trash, Some(b));
}

#[test]
fn shift_delete_does_not_trash_and_a_rebind_wins_over_the_arrow_keys() {
    // consume_key treated Shift+Delete as Delete. Exact matching does not.
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.md");
    let b = dir.path().join("b.md");
    fs::write(&a, "a\n").unwrap();
    fs::write(&b, "b\n").unwrap();
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_rect(&b).is_some());
    h.click(h.browser.row_rect(&b).unwrap().center());
    let output = h.frame(vec![Event::Key {
        key: Key::Delete,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::SHIFT,
    }]);
    assert!(output.trash.is_none());

    // Rename bound to Up has to be taken before Up moves the selection.
    let mut keys = inkmark_view::keys::KeyMap::builtin();
    keys.set(
        inkmark_view::keys::Action::Rename,
        vec![inkmark_view::keys::Chord::parse("ArrowUp").unwrap()],
    );
    h.browser.set_keys(keys);
    let output = h.press(Key::ArrowUp);
    assert_eq!(output.rename, Some(b));
    let output = h.press(Key::F2);
    assert!(output.rename.is_none(), "F2 was given away");
}

#[test]
fn dragging_a_row_onto_a_folder_drops_it_there() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("inner.md"), "").unwrap();
    let b = dir.path().join("b.md");
    fs::write(&b, "b\n").unwrap();
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_rect(&b).is_some() && browser.row_rect(&sub).is_some());

    let (from, to) = (
        h.browser.row_rect(&b).unwrap(),
        h.browser.row_rect(&sub).unwrap(),
    );
    assert_eq!(
        dropped(h.drag(from.center(), to.center())),
        Some((b.clone(), sub.clone()))
    );

    // Onto itself, or onto a sibling in the folder it's already in: no drop.
    let from = h.browser.row_rect(&sub).unwrap();
    assert_eq!(
        dropped(h.drag(from.center(), from.center() + egui::vec2(30.0, 2.0))),
        None
    );
    let from = h.browser.row_rect(&b).unwrap();
    let to = h.browser.row_rect(&sub).unwrap();
    // Dragging b onto itself.
    assert_eq!(
        dropped(h.drag(from.center(), from.center() + egui::vec2(40.0, 0.0))),
        None
    );

    // A file inside `sub`, dropped on empty space below the rows: the root.
    h.click(to.center());
    let inner = sub.join("inner.md");
    h.wait_until(|browser| browser.row_rect(&inner).is_some());
    let from = h.browser.row_rect(&inner).unwrap();
    assert_eq!(
        dropped(h.drag(from.center(), pos2(from.center().x, 500.0))),
        Some((inner, dir.path().to_path_buf()))
    );
}

#[test]
fn the_context_menu_offers_rename_move_trash_and_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    let b = dir.path().join("b.md");
    fs::write(&b, "b\n").unwrap();
    let mut h = Harness::new(dir.path());
    h.wait_until(|browser| browser.row_rect(&b).is_some() && browser.row_rect(&sub).is_some());
    let right_click = |h: &mut Harness, pos: Pos2| {
        let button = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        // Hover first, as a real pointer does after the last menu closed.
        h.frame(vec![Event::PointerMoved(pos)]);
        h.frame(vec![button(true)]);
        h.frame(vec![button(false)]);
        h.frame(vec![]);
    };
    let choose = |h: &mut Harness, row: &Path, item: &str| -> BrowserOutput {
        right_click(h, h.browser.row_rect(row).unwrap().center());
        let at = h.text_rect(item).unwrap_or_else(|| {
            let mut texts = Vec::new();
            fn all(shape: &egui::Shape, out: &mut Vec<String>) {
                match shape {
                    egui::Shape::Text(t) => out.push(t.galley.text().to_owned()),
                    egui::Shape::Vec(v) => v.iter().for_each(|s| all(s, out)),
                    _ => {}
                }
            }
            h.shapes.iter().for_each(|c| all(&c.shape, &mut texts));
            panic!("no {item:?} in the menu; drawn: {texts:?}")
        });
        h.click(at.center())
    };
    assert_eq!(choose(&mut h, &b, "Rename…").rename, Some(b.clone()));
    assert_eq!(choose(&mut h, &b, "Move to…").move_to, Some(b.clone()));
    assert_eq!(choose(&mut h, &b, "Move to Trash").trash, Some(b.clone()));
    let output = choose(&mut h, &sub, "New file here…");
    assert!(output.new_file);
    assert_eq!(h.browser.new_file_dir(), sub);
}
