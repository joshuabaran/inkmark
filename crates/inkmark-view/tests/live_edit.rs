//! M4: editing in the live pane produces minimal source patches, and undo is
//! shared with the code pane. Both panes run side by side on one document,
//! as in the app's split mode.

use std::sync::Arc;
use std::time::Duration;

use egui::{
    Event, Id, ImeEvent, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, UiBuilder, pos2,
};
use inkmark_buffer::{Document, Selection};
use inkmark_parse::{ParseState, PulldownParser};
use inkmark_view::{CodeView, LiveView};

const SCREEN: Rect = Rect::from_min_max(Pos2::ZERO, pos2(1600.0, 600.0));
/// The live pane is the right half.
const LIVE_X: f32 = 800.0;

struct Split {
    ctx: egui::Context,
    code: CodeView,
    live: LiveView,
    doc: Document,
    parse: ParseState,
    time: f64,
}

impl Split {
    fn new(text: &str) -> Self {
        let ctx = egui::Context::default();
        let fonts = inkmark_text::Fonts::shared(&ctx);
        let doc = Document::from_text(text);
        let parse = ParseState::new(Arc::new(PulldownParser), &doc, || {});
        let mut s = Self {
            code: CodeView::with_fonts(fonts.clone(), Id::new("code")),
            live: LiveView::with_fonts(fonts, Id::new("live")),
            ctx,
            doc,
            parse,
            time: 0.0,
        };
        s.live.request_focus(&s.ctx);
        s.settle();
        s
    }

    fn settle(&mut self) {
        while !self.parse.is_settled() {
            self.frame(vec![]);
            std::thread::sleep(Duration::from_millis(2));
        }
        self.frame(vec![]);
    }

    fn frame(&mut self, events: Vec<Event>) {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(SCREEN),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        let (code, live, doc, parse) = (
            &mut self.code,
            &mut self.live,
            &mut self.doc,
            &mut self.parse,
        );
        let mut out = self.ctx.run_ui(input, |ui| {
            let left = Rect::from_min_max(SCREEN.min, pos2(LIVE_X, SCREEN.bottom()));
            let right = Rect::from_min_max(pos2(LIVE_X, 0.0), SCREEN.max);
            ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
                code.show(ui, doc, Some(parse));
            });
            ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
                live.show(ui, doc, Some(parse));
            });
        });
        out.textures_delta.clear();
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) {
        self.frame(vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }]);
    }

    fn press(&mut self, key: Key) {
        self.key(key, Modifiers::NONE);
    }

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.frame(vec![Event::Text(ch.to_string())]);
        }
    }

    fn click(&mut self, pos: Pos2) {
        let button = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        self.frame(vec![Event::PointerMoved(pos), button(true)]);
        self.frame(vec![button(false)]);
    }

    fn text(&self) -> String {
        self.doc.slice(0..self.doc.len()).into_owned()
    }

    /// Puts the live caret at `offset` (as the app does when switching panes).
    fn caret(&mut self, offset: usize) {
        self.live.set_selection(Selection::caret(offset));
        self.frame(vec![]);
    }
}

#[test]
fn typing_patches_the_source_at_the_caret() {
    let src = "Some **bold** text\n";
    let mut s = Split::new(src);
    s.caret(src.find("bold").unwrap() + 2);
    s.type_text("XY");
    assert_eq!(s.text(), "Some **boXYld** text\n");
}

#[test]
fn fuzzed_typing_only_ever_inserts_the_typed_text() {
    let src = "# Title *em*\n\nSome **bold** text, `code` and a [link](http://x).\nSecond line &amp; more.\n\n\
               > quote with *emphasis*\n> - nested item\n\n- one\n- two\n\n```\ncode block\n```\n\nLast para\n";
    let mut s = Split::new(src);
    let mut seed = 0x5eed_u64;
    let mut rand = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    // FUZZ_ITERS=5000 for a longer run.
    let iters = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    for i in 0..iters {
        // Click somewhere in the live pane, then type one char.
        let pos = pos2(LIVE_X + 20.0 + rand(700) as f32, 10.0 + rand(560) as f32);
        s.click(pos);
        let before = s.text();
        let caret = s.live.selection();
        let ch = ['x', 'é', ' ', '中'][rand(4) as usize];
        s.type_text(&ch.to_string());
        let after = s.text();
        let at = caret.range().start;
        assert!(caret.is_empty(), "iteration {i}: a click left a selection");
        let mut expected = before.clone();
        expected.insert(at, ch);
        assert_eq!(
            after, expected,
            "iteration {i}: typing {ch:?} at {at} changed more than that"
        );
        assert_eq!(s.live.selection(), Selection::caret(at + ch.len_utf8()));
        // The parse keeps up: the map always covers the document.
        s.parse.output().map.validate(s.doc.len()).unwrap();
    }
}

#[test]
fn enter_continues_and_ends_lists() {
    let src = "- one\n";
    let mut s = Split::new(src);
    s.caret(5);
    s.press(Key::Enter);
    s.type_text("two");
    assert_eq!(s.text(), "- one\n- two\n");
    s.press(Key::Enter);
    s.press(Key::Enter);
    s.type_text("after");
    assert_eq!(s.text(), "- one\n- two\n\nafter\n");
}

#[test]
fn enter_in_a_paragraph_starts_a_new_one() {
    let mut s = Split::new("Hello world\n");
    s.caret(5);
    s.press(Key::Enter);
    assert_eq!(s.text(), "Hello\n\n world\n");
}

#[test]
fn backspace_removes_what_is_shown() {
    // Deleting the space shown between the two lines of a quoted item joins them.
    let src = "> - one\n>   two\n";
    let mut s = Split::new(src);
    s.caret(src.find("two").unwrap());
    s.press(Key::Backspace);
    assert_eq!(s.text(), "> - onetwo\n");
    // At the start of an item's text, Backspace removes the bullet.
    let mut s = Split::new("- item\n");
    s.caret(2);
    s.press(Key::Backspace);
    assert_eq!(s.text(), "item\n");
}

#[test]
fn formatting_shortcuts() {
    let mut s = Split::new("make this bold\n");
    s.live.set_selection(Selection {
        anchor: 10,
        head: 14,
    });
    s.frame(vec![]);
    s.key(Key::B, Modifiers::COMMAND);
    assert_eq!(s.text(), "make this **bold**\n");
    s.key(Key::B, Modifiers::COMMAND);
    assert_eq!(s.text(), "make this bold\n");
    s.key(Key::Num2, Modifiers::COMMAND.plus(Modifiers::ALT));
    assert_eq!(s.text(), "## make this bold\n");
}

#[test]
fn undo_is_shared_between_panes() {
    let mut s = Split::new("abc\n");
    s.caret(3);
    s.type_text("L");
    // Switch to the code pane and type there.
    s.code.set_selection(Selection::caret(0));
    s.code.request_focus(&s.ctx);
    s.frame(vec![]);
    s.type_text("C");
    assert_eq!(s.text(), "CabcL\n");
    // Undo from the live pane undoes the code edit first, then its own.
    s.live.request_focus(&s.ctx);
    s.frame(vec![]);
    s.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(s.text(), "abcL\n");
    s.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(s.text(), "abc\n");
    s.key(Key::Z, Modifiers::COMMAND.plus(Modifiers::SHIFT));
    assert_eq!(s.text(), "abcL\n");
}

#[test]
fn ime_commits_into_the_source() {
    let mut s = Split::new("x\n");
    s.caret(1);
    s.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "にほ".into(),
        active_range_chars: None,
    })]);
    assert_eq!(s.text(), "x\n");
    s.frame(vec![Event::Ime(ImeEvent::Commit("日本".into()))]);
    assert_eq!(s.text(), "x日本\n");
}

#[test]
fn fuzzed_mixed_editing_keeps_invariants() {
    let src = "# Title *em*\n\nSome **bold** text, `code` and a [link](http://x).\n\n\
               > quote with *emphasis*\n> - nested item\n\n1. one\n2. two\n\n```\ncode\n```\n\nEnd\n";
    let mut s = Split::new(src);
    let mut seed = 0xfeed_u64;
    let mut rand = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let cmd = Modifiers::COMMAND;
    let iters = std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    for i in 0..iters {
        match rand(16) {
            0 | 1 => s.click(pos2(
                LIVE_X + 20.0 + rand(700) as f32,
                10.0 + rand(560) as f32,
            )),
            2..=4 => s.type_text(["a", "*", "#", " ", "-", ">", "`", "é"][rand(8) as usize]),
            5 => s.press(Key::Enter),
            6 => s.key(Key::Enter, Modifiers::SHIFT),
            7 => s.press(Key::Backspace),
            8 => s.press(Key::Delete),
            9 => s.key(
                Key::Tab,
                if rand(2) == 0 {
                    Modifiers::NONE
                } else {
                    Modifiers::SHIFT
                },
            ),
            10 => s.press(
                [
                    Key::ArrowLeft,
                    Key::ArrowRight,
                    Key::ArrowUp,
                    Key::ArrowDown,
                ][rand(4) as usize],
            ),
            11 => s.key(Key::ArrowRight, Modifiers::SHIFT),
            12 => s.key(
                [Key::B, Key::I, Key::Backtick, Key::K][rand(4) as usize],
                cmd,
            ),
            13 => s.key(Key::Z, cmd),
            14 => s.key(Key::Z, cmd.plus(Modifiers::SHIFT)),
            _ => s.key(Key::Num2, cmd.plus(Modifiers::ALT)),
        }
        s.parse
            .output()
            .map
            .validate(s.doc.len())
            .unwrap_or_else(|e| panic!("iteration {i}: {e}\n{:?}", s.text()));
        let sel = s.live.selection();
        assert!(
            sel.anchor <= s.doc.len() && sel.head <= s.doc.len(),
            "iteration {i}"
        );
    }
    eprintln!(
        "{iters} ops: epoch {}, final length {}, focused {}",
        s.doc.epoch(),
        s.doc.len(),
        s.live.has_focus(&s.ctx)
    );
    assert!(s.doc.epoch() > iters as u64 / 4, "too few edits landed");
    while s.doc.undo().is_some() {}
    assert_eq!(s.text(), src, "undoing everything restores the original");
}

/// Keystroke cost in the live pane inside the longest paragraph of the
/// Tolstoy book (or a synthetic 3 KB paragraph). Run with:
/// `cargo test --release -p inkmark-view --test live_edit -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_live_typing_long_paragraph() {
    let book = std::env::var("HOME")
        .ok()
        .and_then(|h| std::fs::read_to_string(format!("{h}/Projects/inkmark/tolstoy.md")).ok())
        .unwrap_or_else(|| format!("{}\n", "word ".repeat(600)));
    let longest = book
        .split("\n\n")
        .max_by_key(|p| p.len())
        .unwrap()
        .to_owned();
    let mut s = Split::new(&book);
    let at = book.find(&longest).unwrap() + longest.len() / 2;
    let at = (at..).find(|&i| book.is_char_boundary(i)).unwrap();
    let parse = s.parse.output().clone();
    let line = s.doc.byte_to_line(at);
    s.live
        .set_scroll_pos(&s.doc, &parse, inkmark_view::ScrollPos { line, frac: 0.0 });
    s.caret(at);
    let mut times = Vec::new();
    for _ in 0..200 {
        let t = std::time::Instant::now();
        s.type_text("x");
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    println!(
        "paragraph of {} bytes: live keystroke→frame ms p50={:.2} p95={:.2} max={:.2}",
        longest.len(),
        times[times.len() / 2],
        times[times.len() * 95 / 100],
        times[times.len() - 1],
    );
}
