//! M4: editing in the live pane produces minimal source patches, and undo is
//! shared with the code pane. Both panes run side by side on one document,
//! as in the app's split mode.

use std::sync::Arc;
use std::time::Duration;

use egui::{
    Event, Id, ImeEvent, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, UiBuilder, pos2,
};
use inkmark_buffer::{Document, Selection};
use inkmark_parse::{GfmParser, ParseState};
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
    /// Modifier keys held during the next frames (e.g. Ctrl for Ctrl+click).
    held: Modifiers,
    /// What the last frame drew, to find menu items by their text.
    shapes: Vec<egui::epaint::ClippedShape>,
}

impl Split {
    fn new(text: &str) -> Self {
        let ctx = egui::Context::default();
        let fonts = inkmark_text::Fonts::shared(&ctx);
        let doc = Document::from_text(text);
        // As in the app: GFM.
        let parse = ParseState::new(Arc::new(GfmParser), &doc, || {});
        let mut s = Self {
            code: CodeView::with_fonts(fonts.clone(), Id::new("code")),
            live: LiveView::with_fonts(fonts, Id::new("live")),
            ctx,
            doc,
            parse,
            time: 0.0,
            held: Modifiers::NONE,
            shapes: Vec::new(),
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

    /// One frame with only one pane shown, mirroring the app's single-pane
    /// modes: the shown pane's caret is copied onto the hidden one.
    fn frame_one(&mut self, events: Vec<Event>, code: bool) {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(SCREEN),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        let (code_view, live, doc, parse) = (
            &mut self.code,
            &mut self.live,
            &mut self.doc,
            &mut self.parse,
        );
        let mut out = self.ctx.run_ui(input, |ui| {
            if code {
                code_view.show(ui, doc, Some(parse));
            } else {
                live.show(ui, doc, Some(parse));
            }
        });
        out.textures_delta.clear();
        if code {
            self.live.mirror_selection(self.code.selection());
        } else {
            self.code.mirror_selection(self.live.selection());
        }
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
        self.shapes = out.shapes;
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

    /// Holds (or with `NONE`, releases) modifier keys from the next frame.
    fn hold(&mut self, modifiers: Modifiers) {
        self.held = modifiers;
        self.frame(vec![Event::ModifiersChanged(modifiers)]);
    }

    fn click(&mut self, pos: Pos2) {
        let held = self.held;
        let button = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: held,
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
               > quote with *emphasis*\n> - nested item\n\n- one\n- two\n\n```\ncode block\n```\n\n\
               A note[^1] here.\n\n[^1]: The *note*\n    goes on.\n\nLast para\n";
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
    // Exactly one blank line ends the list (#1); the file's final newline
    // became it.
    assert_eq!(s.text(), "- one\n- two\n\nafter");
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
fn extra_shift_does_not_bold() {
    // The old match was `Key::B if command`, so Ctrl+Shift+B bolded too.
    let mut s = Split::new("make this bold\n");
    s.live.set_selection(Selection {
        anchor: 10,
        head: 14,
    });
    s.frame(vec![]);
    s.key(Key::B, Modifiers::COMMAND.plus(Modifiers::SHIFT));
    assert_eq!(s.text(), "make this bold\n");
    s.key(Key::B, Modifiers::COMMAND);
    assert_eq!(s.text(), "make this **bold**\n");
}

#[test]
fn a_rebound_chord_replaces_the_default_and_an_empty_list_unbinds() {
    let mut s = Split::new("make this bold\n");
    let mut keys = inkmark_view::keys::KeyMap::builtin();
    keys.set(
        inkmark_view::keys::Action::Bold,
        vec![inkmark_view::keys::Chord::parse("Ctrl+G").unwrap()],
    );
    s.live.set_keys(keys);
    s.live.set_selection(Selection {
        anchor: 10,
        head: 14,
    });
    s.frame(vec![]);
    s.key(Key::B, Modifiers::COMMAND);
    assert_eq!(s.text(), "make this bold\n", "Ctrl+B was given away");
    s.key(Key::G, Modifiers::COMMAND);
    assert_eq!(s.text(), "make this **bold**\n");

    let mut keys = inkmark_view::keys::KeyMap::builtin();
    keys.set(inkmark_view::keys::Action::WordRight, vec![]);
    s.live.set_keys(keys);
    s.caret(0);
    s.key(Key::ArrowRight, Modifiers::COMMAND);
    assert_eq!(
        s.live.selection().head,
        1,
        "an unbound Ctrl+Right moves one character"
    );
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
               > quote with *emphasis*\n> - nested item\n\n1. one\n2. two\n\n```\ncode\n```\n\n\
               | a | b |\n|:--|--:|\n| c | ~~d~~ |\n\n- [ ] task www.x.com\n\nSee[^n].\n\n[^n]: A note.\n\nEnd\n";
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
    let book = inkmark_bench::fixtures::load("prose-5mb.md");
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

#[test]
fn clicking_a_checkbox_toggles_the_task() {
    let mut s = Split::new("- [ ] write tests\n- [x] ship\n");
    s.caret(s.doc.len());
    let before = s.live.selection();
    // The first item's checkbox sits where its bullet would be.
    s.click(pos2(LIVE_X + 38.0, 13.0));
    assert_eq!(s.text(), "- [x] write tests\n- [x] ship\n");
    assert_eq!(
        s.live.selection(),
        before,
        "a checkbox click doesn't move the caret"
    );
    s.click(pos2(LIVE_X + 38.0, 13.0));
    assert_eq!(s.text(), "- [ ] write tests\n- [x] ship\n");
}

#[test]
fn table_cells_edit_and_tab_between() {
    let src = "| a | b |\n|---|---|\n| c | d |\n";
    let mut s = Split::new(src);
    s.caret(src.find('a').unwrap() + 1);
    s.type_text("X");
    assert_eq!(s.text(), "| aX | b |\n|---|---|\n| c | d |\n");
    // A typed pipe can't split the cell.
    s.type_text("|");
    assert_eq!(s.text(), "| aX\\| | b |\n|---|---|\n| c | d |\n");
    s.press(Key::Tab);
    s.type_text("Y");
    assert_eq!(s.text(), "| aX\\| | Yb |\n|---|---|\n| c | d |\n");
    s.key(Key::Tab, Modifiers::SHIFT);
    assert_eq!(
        s.live.selection(),
        Selection::caret(2),
        "back to the first cell's text"
    );
    // Tab through to the last cell, then once more adds a row.
    for _ in 0..3 {
        s.press(Key::Tab);
    }
    s.press(Key::Tab);
    s.type_text("e");
    assert_eq!(s.text(), "| aX\\| | Yb |\n|---|---|\n| c | d |\n| e |  |\n");
}

#[test]
fn enter_moves_down_a_column_and_adds_rows() {
    let src = "| h1 | h2 |\n|:---|---:|\n| a  | b  |\n";
    let mut s = Split::new(src);
    s.caret(src.find("h2").unwrap());
    s.press(Key::Enter);
    assert_eq!(
        s.live.selection(),
        Selection::caret(src.find(" b").unwrap() + 1)
    );
    s.press(Key::Enter);
    s.type_text("z");
    assert_eq!(s.text(), format!("{src}| z |  |\n"));
}

#[test]
fn up_and_down_move_between_table_rows() {
    let src = "| left | right |\n|------|-------|\n| one  | two   |\n| three | four |\n";
    let mut s = Split::new(src);
    s.caret(src.find("two").unwrap() + 1);
    s.press(Key::ArrowDown);
    let head = s.live.selection().head;
    assert!(
        (src.find("four").unwrap()..=src.find("four").unwrap() + 4).contains(&head),
        "down stays in the right-hand column: {head}"
    );
    s.press(Key::ArrowUp);
    s.press(Key::ArrowUp);
    let head = s.live.selection().head;
    assert!(
        (src.find("right").unwrap()..=src.find("right").unwrap() + 5).contains(&head),
        "up reaches the header cell: {head}"
    );
}

#[test]
fn pasted_or_committed_pipes_stay_inside_the_cell() {
    // Regression for #4.
    let src = "| a | b |\n|---|---|\n";
    let mut s = Split::new(src);
    s.caret(src.find('a').unwrap() + 1);
    s.frame(vec![Event::Paste("x|y\nz".into())]);
    assert_eq!(s.text(), "| ax\\|y z | b |\n|---|---|\n");
    s.frame(vec![Event::Ime(ImeEvent::Commit("|".into()))]);
    assert_eq!(s.text(), "| ax\\|y z\\| | b |\n|---|---|\n");
}

#[test]
fn backspace_and_delete_stop_at_a_cells_edges() {
    // Regression for #14.
    let src = "| a | b |\n|---|---|\n| c | d |\n";
    let mut s = Split::new(src);
    s.caret(src.find('a').unwrap());
    s.press(Key::Backspace);
    assert_eq!(s.text(), src);
    s.caret(src.find('b').unwrap() + 1);
    s.press(Key::Delete);
    assert_eq!(s.text(), src);
    // Inside the text they still work.
    s.press(Key::Backspace);
    assert_eq!(s.text(), "| a |  |\n|---|---|\n| c | d |\n");
}

#[test]
fn tab_on_a_short_row_skips_placeholder_cells() {
    // Regression for #12.
    let src = "| a | b | c |\n|---|---|---|\n| d | e |\n| f |\n";
    let mut s = Split::new(src);
    s.caret(src.find('e').unwrap() + 1);
    s.press(Key::Tab);
    s.type_text("Z");
    assert_eq!(
        s.text(),
        "| a | b | c |\n|---|---|---|\n| d | e |\n| Zf |\n"
    );
}

#[test]
fn enter_with_a_selection_in_a_cell_keeps_the_table() {
    // Regression for #10.
    let src = "| a | b |\n|---|---|\n| c | d |\n";
    let mut s = Split::new(src);
    let a = src.find('a').unwrap();
    s.live.set_selection(Selection {
        anchor: a,
        head: a + 1,
    });
    s.frame(vec![]);
    s.press(Key::Enter);
    assert_eq!(s.text(), src);
    assert_eq!(s.live.selection(), Selection::caret(src.find('c').unwrap()));
}

#[test]
fn a_caret_move_ends_the_undo_group() {
    // Regression for #11.
    let mut s = Split::new("hello");
    s.caret(5);
    s.type_text("ab");
    s.press(Key::ArrowLeft);
    s.press(Key::ArrowRight);
    s.type_text("c");
    s.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(s.text(), "helloab");
}

#[test]
fn a_hidden_pane_takes_the_caret_as_it_is() {
    // Regression for #13: the hidden pane must not map the caret it was
    // handed through the same edits a second time.
    let mut s = Split::new("hello\n");
    s.code.request_focus(&s.ctx);
    s.frame_one(vec![], true);
    s.code.set_selection(Selection::caret(0));
    for ch in ["X", "Y", "Z"] {
        s.frame_one(vec![Event::Text(ch.into())], true);
    }
    assert_eq!(s.code.selection().head, 3);
    s.live.request_focus(&s.ctx);
    for _ in 0..4 {
        s.frame_one(vec![], false);
    }
    assert_eq!(s.live.selection().head, 3);
}

#[test]
fn a_hidden_pane_takes_the_scroll_position_as_it_is() {
    // Regression for #13, scroll half: the position handed to the hidden
    // pane is already current. This document is one long paragraph, which
    // also checks positions inside a block that hasn't been drawn yet.
    let text: String = (0..50).map(|i| format!("line {i}\n")).collect();
    let mut s = Split::new(&text);
    let parse = s.parse.output().clone();
    s.code.set_scroll_pos(inkmark_view::ScrollPos {
        line: 40,
        frac: 0.0,
    });
    s.live.set_scroll_pos(
        &s.doc,
        &parse,
        inkmark_view::ScrollPos {
            line: 40,
            frac: 0.0,
        },
    );
    s.frame(vec![]);
    for _ in 0..2 {
        s.doc
            .apply(
                vec![inkmark_buffer::Edit::insert(0, "\n")],
                Selection::caret(0),
                Selection::caret(0),
                inkmark_buffer::EditKind::Other,
            )
            .unwrap();
    }
    s.frame_one(vec![], true);
    let pos = s.code.scroll_pos();
    assert!(
        pos.line >= 42,
        "the shown pane follows the new lines: {pos:?}"
    );
    let parse = s.parse.output().clone();
    s.live.set_scroll_pos(&s.doc, &parse, pos);
    s.live.request_focus(&s.ctx);
    for _ in 0..4 {
        s.frame_one(vec![], false);
    }
    let parse = s.parse.output().clone();
    assert_eq!(s.live.scroll_pos(&s.doc, &parse).line, pos.line);
}

#[test]
fn shift_enter_in_a_table_explains_why_it_does_nothing() {
    // From the first review's suggestions: ignoring it is safe, but silent.
    let src = "| a | b |\n|---|---|\n| c | d |\n";
    let mut s = Split::new(src);
    s.caret(src.find('c').unwrap());
    assert_eq!(s.live.take_hint(), None);
    s.key(Key::Enter, Modifiers::SHIFT);
    assert_eq!(s.text(), src);
    assert!(s.live.take_hint().is_some());
    assert_eq!(s.live.take_hint(), None, "shown once");
}

#[test]
fn typing_around_footnotes_patches_the_source() {
    let src = "Text[^1] more.\n\n[^1]: The note.\n";
    let mut s = Split::new(src);
    // Just before and just after the reference.
    s.caret(4);
    s.type_text("A");
    assert_eq!(s.text(), "TextA[^1] more.\n\n[^1]: The note.\n");
    let after = s.text().find(" more").unwrap();
    s.caret(after);
    s.type_text("B");
    assert_eq!(s.text(), "TextA[^1]B more.\n\n[^1]: The note.\n");
    // Inside the definition's text.
    let note = s.text().find("note.").unwrap();
    s.caret(note);
    s.type_text("good ");
    assert_eq!(s.text(), "TextA[^1]B more.\n\n[^1]: The good note.\n");
}

#[test]
fn ctrl_click_on_a_link_reports_it_and_leaves_the_caret() {
    let src = "[a fairly long link text](other.md) after\n\nMore text.\n";
    let mut s = Split::new(src);
    let caret = src.find("More").unwrap();
    s.caret(caret);
    let on_link = pos2(LIVE_X + 60.0, 13.0);
    // A plain click moves the caret into the link text, as usual.
    s.click(on_link);
    assert!(s.live.selection().head < src.find("](").unwrap());
    assert_eq!(s.live.take_follow(), None);
    s.caret(caret);
    s.hold(Modifiers::COMMAND);
    s.click(on_link);
    s.hold(Modifiers::NONE);
    let at = s.live.take_follow().expect("Ctrl+click follows the link");
    assert!(at < src.find("](").unwrap(), "{at}");
    assert_eq!(
        s.live.selection(),
        Selection::caret(caret),
        "the caret stays"
    );
    assert_eq!(s.live.take_follow(), None, "taken once");
    // Ctrl+click off any link is an ordinary click.
    s.hold(Modifiers::COMMAND);
    s.click(pos2(LIVE_X + 60.0, SCREEN.bottom() - 20.0));
    s.hold(Modifiers::NONE);
    assert_eq!(s.live.take_follow(), None);
    // The code pane follows too.
    s.hold(Modifiers::COMMAND);
    s.click(pos2(30.0, 10.0));
    s.hold(Modifiers::NONE);
    assert!(s.code.take_follow().is_some());
}

#[test]
fn home_and_end_go_to_the_wrapped_rows_edges() {
    // Wider than the live pane, so it wraps into several rows.
    let para = "word ".repeat(60).trim_end().to_owned();
    let src = format!("{para}\n\nNext.\n");
    let mut s = Split::new(&src);
    let mid = para.len() / 2;
    s.caret(mid);
    s.press(Key::Home);
    let row_start = s.live.selection().head;
    assert!(row_start > 0 && row_start <= mid, "row start {row_start}");
    s.press(Key::End);
    let row_end = s.live.selection().head;
    assert!(row_end >= mid && row_end < para.len(), "row end {row_end}");
    // Home again from the row's end comes back to the same row start.
    s.press(Key::Home);
    assert_eq!(s.live.selection().head, row_start);
    s.key(Key::End, Modifiers::COMMAND);
    assert_eq!(s.live.selection().head, src.len());
    s.key(Key::Home, Modifiers::COMMAND.plus(Modifiers::SHIFT));
    assert_eq!(
        s.live.selection().range(),
        0..src.len(),
        "Ctrl+Shift+Home selects to the start"
    );
    // Word steps in the live pane.
    s.caret(0);
    s.key(Key::ArrowRight, Modifiers::COMMAND);
    assert_eq!(s.live.selection().head, 4);
}

#[test]
fn a_table_wider_than_the_pane_still_edits_its_last_column() {
    let long = "a long cell with many words in it ".repeat(4);
    let src = format!("| one | {long} | three |\n|---|---|---|\n| x | y | z |\n");
    let mut s = Split::new(&src);
    // After the `z` in the last cell.
    let z = src.find("| z").unwrap() + 3;
    s.caret(z);
    s.type_text("Z");
    assert!(s.text().contains("| zZ |"), "{}", s.text());
    // Shift+Tab: the start of the cell before.
    s.key(Key::Tab, Modifiers::SHIFT);
    s.type_text("Y");
    assert!(s.text().contains("| Yy |"), "{}", s.text());
}

#[test]
fn end_on_a_mid_word_wrap_stays_on_the_row() {
    // Review of #28, in the live pane: a long unbroken word.
    let long = "x".repeat(300);
    let src = format!("{long}\n\nNext.\n");
    let mut s = Split::new(&src);
    s.caret(0);
    s.press(Key::End);
    let row_end = s.live.selection().head;
    assert!(row_end > 10 && row_end < 300, "wrapped: {row_end}");
    s.press(Key::End);
    assert_eq!(s.live.selection().head, row_end, "End again stays");
    // The next row starts exactly there: End reached the row's true end.
    s.press(Key::ArrowRight);
    s.press(Key::Home);
    assert_eq!(
        s.live.selection().head,
        row_end,
        "the next row starts where End stopped"
    );
    s.press(Key::ArrowLeft);
    s.press(Key::End);
    s.press(Key::Home);
    assert_eq!(s.live.selection().head, 0);
    s.press(Key::End);
    s.type_text("Y");
    assert_eq!(&s.text()[row_end..row_end + 1], "Y");
    assert_eq!(
        &s.text()[row_end - 1..row_end],
        "x",
        "after the row's last x"
    );
}

impl Split {
    /// Where the last frame drew `text` (topmost match, e.g. a menu item).
    fn text_rect(&self, text: &str) -> Option<Rect> {
        fn find(shape: &egui::Shape, text: &str) -> Option<Rect> {
            match shape {
                egui::Shape::Text(t) if t.galley.text() == text => {
                    Some(t.galley.rect.translate(t.pos.to_vec2()))
                }
                egui::Shape::Vec(v) => v.iter().find_map(|s| find(s, text)),
                _ => None,
            }
        }
        self.shapes.iter().rev().find_map(|c| find(&c.shape, text))
    }

    fn right_click(&mut self, pos: Pos2) {
        let button = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Secondary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        self.frame(vec![Event::PointerMoved(pos)]);
        self.frame(vec![button(true)]);
        self.frame(vec![button(false)]);
        self.frame(vec![]);
    }

    fn click_text(&mut self, text: &str) {
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
        self.frame(vec![Event::PointerMoved(at)]);
        self.click(at);
        self.frame(vec![]);
    }
}

const TABLE: &str = "Intro.\n\n| a | b |\n|---|---|\n| c | d |\n\nAfter.\n";

#[test]
fn table_shortcuts_work_in_the_live_pane() {
    let mut s = Split::new(TABLE);
    s.caret(TABLE.find('c').unwrap());
    s.key(Key::ArrowDown, Modifiers::COMMAND.plus(Modifiers::ALT));
    assert_eq!(
        s.text(),
        "Intro.\n\n| a   | b   |\n| --- | --- |\n| c   | d   |\n|     |     |\n\nAfter.\n"
    );
    s.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(s.text(), TABLE, "one undo step");
    s.caret(TABLE.find('c').unwrap());
    s.key(Key::ArrowRight, Modifiers::ALT.plus(Modifiers::SHIFT));
    assert!(s.text().contains("| d   | c   |"), "{}", s.text());
    s.key(Key::Z, Modifiers::COMMAND);
    // Outside a table, Alt+Shift+Right still extends the selection.
    s.caret(0);
    s.key(Key::ArrowRight, Modifiers::ALT.plus(Modifiers::SHIFT));
    assert_eq!(s.text(), TABLE);
    assert!(!s.live.selection().range().is_empty());
}

#[test]
fn shift_on_insert_column_selects_a_word_instead() {
    // Review of #34. Ctrl+Alt+Shift+Left is not insert-column, in a table
    // or out of one. It selects a word, as Ctrl+Shift+Left does.
    let ctrl_alt_shift = Modifiers::COMMAND
        .plus(Modifiers::ALT)
        .plus(Modifiers::SHIFT);
    let mut s = Split::new("alpha beta gamma\n");
    s.caret(0);
    s.key(Key::ArrowRight, ctrl_alt_shift);
    assert_eq!(s.text(), "alpha beta gamma\n");
    assert_eq!(s.live.selection().range(), 0..5);

    let mut s = Split::new(TABLE);
    s.caret(TABLE.find('c').unwrap());
    let before = s.text().clone();
    s.key(Key::ArrowLeft, ctrl_alt_shift);
    assert_eq!(s.text(), before, "Shift does not insert a column");
}

#[test]
fn leaving_an_edited_table_re_pads_it_as_its_own_undo_step() {
    let mut s = Split::new(TABLE);
    let c = TABLE.find('c').unwrap();
    s.caret(c + 1);
    s.type_text("ell");
    assert_eq!(
        s.text(),
        TABLE.replace("| c |", "| cell |"),
        "typing changes only the cell"
    );
    // Leave the table: it's re-padded.
    s.key(Key::End, Modifiers::COMMAND);
    let padded = "Intro.\n\n| a    | b   |\n| ---- | --- |\n| cell | d   |\n\nAfter.\n";
    assert_eq!(s.text(), padded);
    assert_eq!(
        s.live.selection().head,
        padded.len(),
        "the caret is still at the end"
    );
    s.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(
        s.text(),
        TABLE.replace("| c |", "| cell |"),
        "undo the re-padding alone"
    );
    s.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(s.text(), TABLE);

    // Only passing through a table doesn't touch it.
    let mut s = Split::new(TABLE);
    s.caret(TABLE.find('c').unwrap());
    s.press(Key::ArrowRight);
    s.key(Key::End, Modifiers::COMMAND);
    assert_eq!(s.text(), TABLE);
}

#[test]
fn the_right_click_menu_edits_tables_and_inserts_one() {
    let mut s = Split::new(TABLE);
    // A right-click moves the caret to the table's header row first.
    s.right_click(pos2(LIVE_X + 30.0, 70.0));
    assert!(
        tables_row(&s.text(), s.live.selection().head),
        "the caret moved into the table"
    );
    s.click_text("Insert row below");
    assert_eq!(
        s.text(),
        "Intro.\n\n| a   | b   |\n| --- | --- |\n|     |     |\n| c   | d   |\n\nAfter.\n"
    );

    // Outside a table: Insert table, with its first header selected.
    let mut s = Split::new("Hello.\n");
    s.right_click(pos2(LIVE_X + 30.0, 13.0));
    s.click_text("Insert table");
    assert!(
        s.text().starts_with("Hello.\n\n| Column 1 |"),
        "{}",
        s.text()
    );
    let sel = s.live.selection().range();
    assert_eq!(&s.text()[sel], "Column 1");

    // The menu advertises the chord the table is actually bound to.
    let mut s = Split::new("Hello.\n");
    let mut keys = inkmark_view::keys::KeyMap::builtin();
    keys.set(
        inkmark_view::keys::Action::InsertTable,
        vec![inkmark_view::keys::Chord::parse("Ctrl+G").unwrap()],
    );
    s.live.set_keys(keys);
    s.right_click(pos2(LIVE_X + 30.0, 13.0));
    assert!(
        s.text_rect("Ctrl+G").is_some(),
        "the menu should show the rebound chord"
    );
    assert!(s.text_rect("Ctrl+Alt+T").is_none());
}

#[test]
fn table_shortcuts_work_in_the_code_pane_too() {
    let mut s = Split::new(TABLE);
    s.code.request_focus(&s.ctx);
    s.code
        .set_selection(Selection::caret(TABLE.find('c').unwrap()));
    s.frame(vec![]);
    s.frame(vec![Event::Key {
        key: Key::F,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND.plus(Modifiers::ALT),
    }]);
    assert!(s.text().contains("| --- | --- |"), "{}", s.text());
    s.frame(vec![Event::Key {
        key: Key::T,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::COMMAND.plus(Modifiers::ALT),
    }]);
    assert!(s.text().contains("| Column 1 |"), "{}", s.text());
}

/// Whether `at` is on one of the table lines of the `TABLE` sample.
fn tables_row(text: &str, at: usize) -> bool {
    text[..at].matches('\n').count() >= 2 && text[at..].contains("After.")
}

#[test]
fn a_quoted_table_is_re_padded_on_leaving() {
    // Review of #32: the visit was keyed by an offset before the block.
    let src = "> | a | b |\n> |---|---|\n> | c | d |\n\nAfter.\n";
    let mut s = Split::new(src);
    s.caret(src.find('c').unwrap() + 1);
    s.type_text("ell");
    s.key(Key::End, Modifiers::COMMAND);
    assert_eq!(
        s.text(),
        "> | a    | b   |\n> | ---- | --- |\n> | cell | d   |\n\nAfter.\n"
    );
}

#[test]
fn editing_a_table_in_the_code_pane_doesnt_re_pad_it() {
    // Review of #32: the live pane re-pads only what it edited itself.
    let mut s = Split::new(TABLE);
    s.code.request_focus(&s.ctx);
    // As the app does in split mode: the live pane mirrors the code caret.
    let frame = |s: &mut Split, events: Vec<Event>| {
        s.frame(events);
        s.live.mirror_selection(s.code.selection());
    };
    s.code
        .set_selection(Selection::caret(TABLE.find('c').unwrap() + 1));
    // Two frames: the live pane sees the caret (mirrored) in the table
    // before the code pane edits it.
    frame(&mut s, vec![]);
    frame(&mut s, vec![]);
    frame(&mut s, vec![Event::Text("ell".into())]);
    s.code.set_selection(Selection::caret(0));
    for _ in 0..3 {
        frame(
            &mut s,
            vec![Event::PointerMoved(pos2(LIVE_X + 50.0, 300.0))],
        );
    }
    assert_eq!(s.text(), TABLE.replace("| c |", "| cell |"));
}

#[test]
fn a_jump_deep_into_a_long_table_draws_the_table() {
    // Eight cells a row make thousands of row and cell blocks, so the
    // table's own block is several chunks before its last rows. The table
    // isn't on screen at open, so nothing has laid it out yet.
    let mut src = "Prose paragraph.\n\n".repeat(200);
    src.push_str("| a | b | c | d | e | f | g | h |\n");
    src.push_str("|---|---|---|---|---|---|---|---|\n");
    for i in 0..500 {
        src.push_str(&format!("| {i} | x | y | z | w | v | u | t |\n"));
    }
    let line_of = |needle: &str| src[..src.find(needle).unwrap()].matches('\n').count();
    let (first, row) = (line_of("| a |"), line_of("| 450 |"));
    let mut s = Split::new(&src);
    assert!(!s.live.line_measured(first), "the table starts unmeasured");
    let parse = s.parse.output().clone();
    s.live.set_scroll_pos(
        &s.doc,
        &parse,
        inkmark_view::ScrollPos {
            line: row,
            frac: 0.0,
        },
    );
    s.frame(vec![]);
    // Drawn as one table: its height sits on its first line and its rows
    // hold nothing. A row drawn as a raw source line holds a height of its
    // own.
    assert!(s.live.line_measured(first), "the table was laid out");
    assert_eq!(
        s.live.measured_height(row),
        0.0,
        "row {row} was drawn as a raw line"
    );
    assert_eq!(s.live.view_top().0, first, "the view is inside the table");
}
