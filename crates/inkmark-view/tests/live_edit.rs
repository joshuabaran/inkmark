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
