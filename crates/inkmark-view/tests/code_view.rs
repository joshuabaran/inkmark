//! Drives `CodeView` through egui with synthetic input, no window needed.

use egui::{
    Event, FullOutput, Id, ImeEvent, Key, Modifiers, MouseWheelUnit, OutputCommand, PointerButton,
    Pos2, RawInput, Rect, TouchPhase, pos2, vec2,
};
use inkmark_buffer::{Document, Edit, EditKind, Selection};
use inkmark_minimap::Minimap;
use inkmark_parse::{ParseState, PulldownParser};
use inkmark_view::{CodeView, ScrollPos};

const SCREEN: Rect = Rect::from_min_max(Pos2::ZERO, pos2(800.0, 600.0));

struct Harness {
    ctx: egui::Context,
    view: CodeView,
    doc: Document,
    parse: Option<ParseState>,
    time: f64,
}

impl Harness {
    fn new(text: &str) -> Self {
        let ctx = egui::Context::default();
        let view = CodeView::new(&ctx, Id::new("code"));
        let mut h = Self {
            ctx,
            view,
            doc: Document::from_text(text),
            parse: None,
            time: 0.0,
        };
        h.view.request_focus(&h.ctx);
        h.frame(vec![]);
        h.frame(vec![]);
        h
    }

    fn frame(&mut self, events: Vec<Event>) -> FullOutput {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(SCREEN),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        let (view, doc, parse) = (&mut self.view, &mut self.doc, &mut self.parse);
        let mut out = self.ctx.run_ui(input, |ui| {
            view.show(ui, doc, parse.as_mut());
        });
        // No renderer here to apply texture uploads; egui asserts they're handled.
        out.textures_delta.clear();
        out
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) -> FullOutput {
        self.frame(vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }])
    }

    fn press(&mut self, key: Key) {
        self.key(key, Modifiers::NONE);
    }

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.press(Key::Enter);
            } else {
                self.frame(vec![Event::Text(ch.to_string())]);
            }
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

    fn head(&self) -> usize {
        self.view.selection().head
    }
}

const CMD: Modifiers = Modifiers::COMMAND;
const SHIFT: Modifiers = Modifiers::SHIFT;

#[test]
fn typing_newlines_and_undo_steps() {
    let mut h = Harness::new("");
    h.type_text("hello\nworld");
    assert_eq!(h.text(), "hello\nworld");
    assert_eq!(h.head(), 11);

    h.key(Key::Z, CMD);
    assert_eq!(h.text(), "hello\n");
    h.key(Key::Z, CMD);
    assert_eq!(h.text(), "hello");
    h.key(Key::Z, CMD);
    assert_eq!(h.text(), "");
    h.key(Key::Z, CMD.plus(SHIFT));
    assert_eq!(h.text(), "hello");
    assert_eq!(h.head(), 5);
}

#[test]
fn arrows_select_and_backspace_deletes_selection() {
    let mut h = Harness::new("one two three");
    h.key(Key::End, CMD);
    h.key(Key::ArrowLeft, CMD.plus(SHIFT));
    assert_eq!(
        h.view.selection(),
        Selection {
            anchor: 13,
            head: 8
        }
    );
    h.press(Key::Backspace);
    assert_eq!(h.text(), "one two ");
    h.key(Key::Backspace, CMD);
    assert_eq!(h.text(), "one ");
    h.press(Key::Home);
    h.press(Key::Delete);
    assert_eq!(h.text(), "ne ");
}

#[test]
fn grapheme_clusters_move_and_delete_as_one() {
    let mut h = Harness::new("ae\u{301}🇯🇵!");
    h.key(Key::End, CMD);
    h.press(Key::ArrowLeft);
    h.press(Key::Backspace);
    assert_eq!(h.text(), "ae\u{301}!");
    h.press(Key::Backspace);
    assert_eq!(h.text(), "a!");
}

#[test]
fn vertical_movement_keeps_column() {
    let mut h = Harness::new("abcdefgh\nab\nabcdefgh");
    h.press(Key::End);
    assert_eq!(h.head(), 8);
    h.press(Key::ArrowDown);
    assert_eq!(h.head(), 11, "clamped to the short line's end");
    h.press(Key::ArrowDown);
    assert_eq!(h.head(), 20, "back to column 8");
    h.press(Key::ArrowUp);
    h.press(Key::ArrowUp);
    assert_eq!(h.head(), 8);
    h.press(Key::ArrowUp);
    assert_eq!(h.head(), 0, "up from the first line goes to the start");
}

#[test]
fn soft_wrapped_rows_are_separate_stops() {
    // One logical line much wider than the 800 px pane.
    let long = "word ".repeat(100);
    let mut h = Harness::new(&format!("{long}\nend"));
    h.press(Key::ArrowDown);
    let first_row_down = h.head();
    assert!(
        first_row_down > 0 && first_row_down < long.len(),
        "moved within the wrapped line: {first_row_down}"
    );
    h.press(Key::Home);
    assert!(
        h.head() > 0,
        "Home goes to the row start, not the line start"
    );
}

#[test]
fn paste_normalizes_newlines_and_copy_cut_use_clipboard() {
    let mut h = Harness::new("");
    h.frame(vec![Event::Paste("a\r\nb\rc".into())]);
    assert_eq!(h.text(), "a\nb\nc");

    h.key(Key::A, CMD);
    let out = h.frame(vec![Event::Cut]);
    assert!(
        out.platform_output
            .commands
            .contains(&OutputCommand::CopyText("a\nb\nc".into()))
    );
    assert_eq!(h.text(), "");
    h.key(Key::Z, CMD);
    assert_eq!(h.text(), "a\nb\nc");
}

#[test]
fn tab_indents_and_shift_tab_outdents_selected_lines() {
    let mut h = Harness::new("a\nb\nc");
    // A selection ending at a line start doesn't include that line.
    h.key(Key::ArrowDown, SHIFT);
    h.key(Key::ArrowDown, SHIFT);
    h.press(Key::Tab);
    assert_eq!(h.text(), "    a\n    b\nc");
    h.key(Key::Tab, SHIFT);
    h.key(Key::End, SHIFT);
    h.press(Key::Tab);
    assert_eq!(h.text(), "    a\n    b\n    c");
    h.key(Key::Tab, SHIFT);
    assert_eq!(h.text(), "a\nb\nc");
    // Single-line: Tab inserts spaces at the caret.
    h.key(Key::Home, CMD);
    h.press(Key::Tab);
    assert_eq!(h.text(), "    a\nb\nc");
}

#[test]
fn ime_composition_only_commits_final_text() {
    let mut h = Harness::new("x");
    h.key(Key::End, CMD);
    h.frame(vec![Event::Ime(ImeEvent::Preedit {
        text: "にほ".into(),
        active_range_chars: None,
    })]);
    assert_eq!(h.text(), "x", "preedit is not part of the document");
    h.frame(vec![Event::Ime(ImeEvent::Commit("日本".into()))]);
    assert_eq!(h.text(), "x日本");
    assert_eq!(h.head(), "x日本".len());
}

#[test]
fn clicks_place_caret_and_double_click_selects_word() {
    let mut h = Harness::new("alpha beta\ngamma");
    // Far right of the first row: end of line 1.
    h.click(pos2(700.0, 8.0));
    assert_eq!(h.head(), 10);
    // Second row, far left: start of line 2.
    h.click(pos2(0.0, 30.0));
    assert_eq!(h.head(), 11);
    // Double-click inside "gamma".
    h.click(pos2(40.0, 30.0));
    h.click(pos2(40.0, 30.0));
    assert_eq!(
        h.view.selection(),
        Selection {
            anchor: 11,
            head: 16
        }
    );
}

#[test]
fn ctrl_end_on_a_large_document_scrolls_caret_into_view() {
    let text: String = (0..100_000).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(&text);
    h.key(Key::End, CMD);
    let mut out = h.frame(vec![]);
    for _ in 0..3 {
        out = h.frame(vec![]);
    }
    assert_eq!(h.head(), h.doc.len());
    let caret = out
        .platform_output
        .ime
        .expect("focused view reports IME rect")
        .cursor_rect;
    assert!(SCREEN.contains_rect(caret), "caret {caret:?} on screen");
}

#[test]
fn edits_from_elsewhere_shift_the_caret() {
    let mut h = Harness::new("hello world");
    h.key(Key::End, CMD);
    // Another pane inserts at the start.
    h.doc
        .apply(
            vec![Edit::insert(0, ">> ")],
            Selection::caret(0),
            Selection::caret(3),
            EditKind::Other,
        )
        .unwrap();
    h.frame(vec![]);
    assert_eq!(h.head(), 14);
    h.type_text("!");
    assert_eq!(h.text(), ">> hello world!");
}

#[test]
fn unfocused_view_ignores_keys() {
    let mut h = Harness::new("abc");
    h.ctx.memory_mut(|m| m.surrender_focus(Id::new("code")));
    h.frame(vec![]);
    h.type_text("zzz");
    assert_eq!(h.text(), "abc");
}

/// Edit latency on a ~5 MB document, mid-file. Run with:
/// `cargo test --release -p inkmark-view --test code_view -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_typing_mid_document_5mb() {
    let line = "Some *typical* prose with a [link](https://example.com) and `code`.\n";
    let text = line.repeat(5_000_000 / line.len());
    let mut h = Harness::new(&text);
    h.parse = Some(ParseState::new(
        std::sync::Arc::new(PulldownParser),
        &h.doc,
        || {},
    ));
    let mid = h.doc.line_to_byte(h.doc.line_count() / 2);
    h.view.set_selection(Selection::caret(mid));
    while !h.parse.as_ref().unwrap().is_settled() {
        h.frame(vec![]);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let before = h.doc.len();
    let mut times = Vec::new();
    for i in 0..300 {
        let event = if i % 40 == 39 {
            Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }
        } else {
            Event::Text("x".into())
        };
        let start = std::time::Instant::now();
        h.frame(vec![event]);
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(h.doc.len(), before + 300, "every keystroke landed");
    let parsed = h.parse.as_ref().unwrap().output();
    parsed.map.validate(h.doc.len()).unwrap();
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    println!(
        "{:.1} MB, {} lines, with parsing: keystroke→frame ms p50={:.2} p95={:.2} max={:.2}",
        h.doc.len() as f64 / 1e6,
        h.doc.line_count(),
        at(0.5),
        at(0.95),
        at(1.0)
    );
    assert!(at(0.95) < 16.0);
}

/// The minimap sits just left of the 10 pt scrollbar on an 800 pt screen.
const MINIMAP_X: f32 = 800.0 - 10.0 - inkmark_minimap::WIDTH / 2.0;

#[test]
fn a_wheel_scrolls_the_code_pane() {
    let text: String = (0..400).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(&text);
    h.ctx.options_mut(|options| {
        options.input_options.line_scroll_speed = 21.0 * 5.0;
    });
    h.frame(vec![
        Event::PointerMoved(pos2(80.0, 80.0)),
        Event::MouseWheel {
            unit: MouseWheelUnit::Line,
            delta: vec2(0.0, -1.0),
            phase: TouchPhase::Move,
            modifiers: Modifiers::NONE,
        },
    ]);
    // Wayland follows the notch with End, which drops egui's unprocessed remainder.
    h.frame(vec![Event::MouseWheel {
        unit: MouseWheelUnit::Line,
        delta: vec2(0.0, 0.0),
        phase: TouchPhase::End,
        modifiers: Modifiers::NONE,
    }]);
    for _ in 0..8 {
        h.frame(vec![]);
    }
    let notch = h.view.scroll_pos().line;
    assert!(notch >= 4, "one wheel notch landed on line {notch}");
}

/// fcitx sends `Preedit ""` on every frame after the cursor rect is set.
/// Re-arming the caret reveal from that echo snaps a notch back to line 0.
#[test]
fn an_empty_preedit_echo_does_not_snap_the_wheel_back_to_line_0() {
    let text: String = (0..400).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(&text);
    h.ctx.options_mut(|options| {
        options.input_options.line_scroll_speed = 21.0 * 5.0;
    });
    let echo = || {
        Event::Ime(ImeEvent::Preedit {
            text: String::new(),
            active_range_chars: None,
        })
    };
    let at = pos2(80.0, 80.0);
    let mut trace = Vec::new();
    for _ in 0..8 {
        h.frame(vec![
            Event::PointerMoved(at),
            echo(),
            Event::MouseWheel {
                unit: MouseWheelUnit::Line,
                delta: vec2(0.0, -1.0),
                phase: TouchPhase::Move,
                modifiers: Modifiers::NONE,
            },
        ]);
        let line = h.view.scroll_pos().line;
        let reveal = h.view.reveal_caret();
        trace.push((line, reveal));
        // A re-armed reveal is decremented in the same frame, so it ends at 2
        // rather than 3, and the anchor is back on line 0.
        assert_eq!(
            (line > 0, reveal),
            (true, 0),
            "notch snapped or reveal stayed armed, trace {trace:?}"
        );
        h.frame(vec![
            Event::PointerMoved(at),
            echo(),
            Event::MouseWheel {
                unit: MouseWheelUnit::Point,
                delta: vec2(0.0, 0.0),
                phase: TouchPhase::Move,
                modifiers: Modifiers::NONE,
            },
        ]);
        let line = h.view.scroll_pos().line;
        let reveal = h.view.reveal_caret();
        trace.push((line, reveal));
        assert_eq!(
            (line > 0, reveal),
            (true, 0),
            "point event snapped or reveal stayed armed, trace {trace:?}"
        );
    }
    let end = trace.last().map(|(line, _)| *line).unwrap_or(0);
    assert!(
        end >= 20,
        "eight notches ended on line {end}, trace {trace:?}"
    );
}

#[test]
fn minimap_click_jumps_and_drag_tracks() {
    let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(&text);
    assert_eq!(h.view.scroll_pos().line, 0);
    // A press near the bottom of the minimap jumps further down the document.
    let press = |pressed| Event::PointerButton {
        pos: pos2(MINIMAP_X, 590.0),
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    h.frame(vec![
        Event::PointerMoved(pos2(MINIMAP_X, 590.0)),
        press(true),
    ]);
    h.frame(vec![press(false)]);
    let jumped = h.view.scroll_pos().line;
    assert!(jumped > 100, "jumped to line {jumped}");
    assert!(h.view.take_scrolled(), "minimap scrolling counts for sync");
    // The minimap is outside the text, so a drag has to arrive on its own
    // widget. Moving down from that jump keeps scrolling.
    h.frame(vec![
        Event::PointerMoved(pos2(MINIMAP_X, 300.0)),
        Event::PointerButton {
            pos: pos2(MINIMAP_X, 300.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        },
    ]);
    let pressed_at = h.view.scroll_pos().line;
    for y in [350.0, 450.0, 600.0] {
        h.frame(vec![Event::PointerMoved(pos2(MINIMAP_X, y))]);
    }
    let dragged = h.view.scroll_pos().line;
    assert!(
        dragged > pressed_at + 50,
        "press landed on {pressed_at}, drag stayed at {dragged}"
    );
    // Grabbing the box and dragging it to the bottom edge reaches the end.
    h.frame(vec![Event::PointerButton {
        pos: pos2(MINIMAP_X, 600.0),
        button: PointerButton::Primary,
        pressed: false,
        modifiers: Modifiers::NONE,
    }]);
    h.view.set_scroll_pos(ScrollPos { line: 0, frac: 0.0 });
    h.frame(vec![
        Event::PointerMoved(pos2(MINIMAP_X, 24.0)),
        Event::PointerButton {
            pos: pos2(MINIMAP_X, 24.0),
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        },
    ]);
    h.frame(vec![Event::PointerMoved(pos2(MINIMAP_X, 600.0))]);
    assert!(
        h.view.scroll_pos().line > 19_900,
        "dragged the box to {}",
        h.view.scroll_pos().line
    );
}

#[test]
fn hiding_the_minimap_widens_the_text() {
    let long = "word ".repeat(40);
    let mut h = Harness::new(&format!("{long}\n"));
    h.press(Key::End);
    let with_minimap = h.head();
    h.view.show_minimap = false;
    h.frame(vec![]);
    h.key(Key::Home, Modifiers::COMMAND);
    h.press(Key::End);
    assert!(
        h.head() > with_minimap,
        "first row holds more text without the minimap"
    );
}

#[test]
fn dragging_the_minimap_box_keeps_it_under_the_pointer() {
    let text: String = (0..8_000).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(&text);
    let minimap = Rect::from_min_max(
        pos2(800.0 - 10.0 - inkmark_minimap::WIDTH, 0.0),
        pos2(800.0 - 10.0, 600.0),
    );
    // Below the box at the top. The jump's document y calibrates the line height.
    let jump_y = 240.0;
    h.click(pos2(MINIMAP_X, jump_y));
    let jumped = h.view.scroll_pos();
    let jumped_lines = jumped.line as f64 + f64::from(jumped.frac);
    assert!(jumped_lines > 1.0, "jump landed on {jumped:?}");
    let expected_y = f64::from(jump_y) / f64::from(inkmark_minimap::SCALE) - 300.0;
    let line_h = expected_y / jumped_lines;
    let total = h.doc.line_count() as f64 * line_h;

    let press_y = 24.0;
    let press = |y, pressed| Event::PointerButton {
        pos: pos2(MINIMAP_X, y),
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    h.view.set_scroll_pos(ScrollPos { line: 0, frac: 0.0 });
    h.frame(vec![
        Event::PointerMoved(pos2(MINIMAP_X, press_y)),
        press(press_y, true),
    ]);
    let held = h.view.scroll_pos();
    assert_eq!(held.line, 0, "a press on the box jumps");
    assert!(held.frac < 0.05, "a press on the box scrolls, {held:?}");

    let dy = 90.0;
    h.frame(vec![Event::PointerMoved(pos2(MINIMAP_X, press_y + dy))]);
    let dragged = h.view.scroll_pos();
    let actual = (dragged.line as f64 + f64::from(dragged.frac)) * line_h;
    let start = Minimap::new(minimap, total, 600.0, 0.0);
    let expected = start.drag_to(press_y + dy, start.grab(press_y));
    assert!(
        (actual - expected).abs() < line_h,
        "scroll {actual} (line {dragged:?}), box-tracking target {expected}, line height {line_h}"
    );

    h.frame(vec![press(press_y + dy, false)]);
    let before = h.head();
    h.press(Key::ArrowRight);
    assert!(h.view.has_focus(&h.ctx), "the pane lost focus");
    assert_ne!(h.head(), before, "the arrow did not move the caret");
    let len = h.doc.len();
    h.type_text("z");
    assert_eq!(h.doc.len(), len + 1, "typing after the drag was lost");
}

#[test]
fn clicking_minimap_or_scrollbar_keeps_typing_focus() {
    let text: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(&text);
    for x in [MINIMAP_X, 795.0] {
        let click = |pressed| Event::PointerButton {
            pos: pos2(x, 300.0),
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        h.frame(vec![Event::PointerMoved(pos2(x, 300.0)), click(true)]);
        h.frame(vec![click(false)]);
        h.frame(vec![]);
        let before = h.doc.len();
        h.type_text("z");
        assert_eq!(
            h.doc.len(),
            before + 1,
            "typing after clicking at x={x} lands"
        );
    }
}

#[test]
fn ctrl_arrows_and_ctrl_delete_work_by_word() {
    let mut h = Harness::new("alpha beta gamma");
    h.key(Key::ArrowRight, CMD);
    assert_eq!(h.head(), 5);
    h.key(Key::ArrowRight, CMD);
    assert_eq!(h.head(), 10);
    h.key(Key::ArrowLeft, CMD);
    assert_eq!(h.head(), 6);
    h.key(Key::ArrowRight, CMD.plus(SHIFT));
    assert_eq!(
        h.view.selection().range(),
        6..10,
        "Ctrl+Shift selects by word"
    );
    h.press(Key::ArrowLeft);
    h.key(Key::Backspace, CMD);
    assert_eq!(h.text(), "beta gamma");
    h.key(Key::Delete, CMD);
    assert_eq!(h.text(), " gamma");
    h.key(Key::Z, CMD);
    h.key(Key::Z, CMD);
    assert_eq!(h.text(), "alpha beta gamma");
}

#[test]
fn ctrl_alt_shift_on_a_column_chord_still_edits_by_word() {
    // Review of #34. Outside a table, Ctrl+Alt+Right moves by word, and
    // adding Shift selects by word. Ctrl+Alt+Shift+Backspace is
    // delete-column, which doesn't apply here, so it deletes a word.
    // Ctrl+Alt+Shift+Delete is not a table chord: extra Alt does not fire
    // delete-word, so one character goes.
    let ctrl_alt = CMD.plus(Modifiers::ALT);
    let ctrl_alt_shift = ctrl_alt.plus(SHIFT);
    let mut h = Harness::new("alpha beta gamma");
    h.key(Key::ArrowRight, ctrl_alt);
    assert_eq!(h.head(), 5, "Ctrl+Alt+Right still moves by word");
    h.press(Key::Home);
    h.key(Key::ArrowRight, ctrl_alt_shift);
    assert_eq!(
        h.view.selection().range(),
        0..5,
        "Ctrl+Alt+Shift+Right selects a word"
    );
    h.press(Key::Home);
    h.key(Key::ArrowRight, CMD);
    h.key(Key::ArrowRight, CMD);
    assert_eq!(h.head(), 10);
    h.key(Key::Backspace, ctrl_alt_shift);
    assert_eq!(h.text(), "alpha  gamma");
    let mut h = Harness::new("alpha beta gamma");
    h.key(Key::Delete, ctrl_alt_shift);
    assert_eq!(h.text(), "lpha beta gamma");
}

#[test]
fn formatting_shortcuts_patch_the_source() {
    let mut h = Harness::new("make this bold\n- [ ] task");
    // Select "this" by word.
    h.key(Key::ArrowRight, CMD);
    h.press(Key::ArrowRight);
    h.key(Key::ArrowRight, CMD.plus(SHIFT));
    h.key(Key::B, CMD);
    assert_eq!(h.text(), "make **this** bold\n- [ ] task");
    h.key(Key::B, CMD);
    assert_eq!(h.text(), "make this bold\n- [ ] task", "toggles off");
    h.key(Key::I, CMD);
    assert_eq!(h.text(), "make *this* bold\n- [ ] task");
    h.key(Key::Z, CMD);
    h.key(Key::Backtick, CMD);
    assert_eq!(h.text(), "make `this` bold\n- [ ] task");
    h.key(Key::Z, CMD);
    h.key(Key::X, CMD.plus(SHIFT));
    assert_eq!(h.text(), "make ~~this~~ bold\n- [ ] task");
    h.key(Key::Z, CMD);
    h.key(Key::K, CMD);
    assert!(h.text().starts_with("make [this]("), "{}", h.text());
    h.key(Key::Z, CMD);
    // Ctrl+Enter ticks the task on the caret's line; Ctrl+Alt+2 makes a heading.
    h.key(Key::End, CMD);
    h.key(Key::Enter, CMD);
    assert_eq!(h.text(), "make this bold\n- [x] task");
    h.key(Key::Home, CMD);
    h.key(Key::Num2, CMD.plus(Modifiers::ALT));
    assert_eq!(h.text(), "## make this bold\n- [x] task");
    // Ctrl+A selects everything.
    h.key(Key::A, CMD);
    assert_eq!(h.view.selection().range(), 0..h.doc.len());
}

#[test]
fn extra_shift_does_not_bold_in_the_code_pane() {
    // The old match was `Key::B if command`, so Ctrl+Shift+B bolded too.
    let mut h = Harness::new("make this bold\n");
    h.key(Key::ArrowRight, CMD);
    h.press(Key::ArrowRight);
    h.key(Key::ArrowRight, CMD.plus(SHIFT));
    h.key(Key::B, CMD.plus(SHIFT));
    assert_eq!(h.text(), "make this bold\n");
    h.key(Key::B, CMD);
    assert_eq!(h.text(), "make **this** bold\n");

    let mut keys = inkmark_view::keys::KeyMap::builtin();
    keys.set(inkmark_view::keys::Action::Bold, vec![]);
    h.view.set_keys(keys);
    h.key(Key::B, CMD);
    assert_eq!(h.text(), "make **this** bold\n", "bold is unbound");
}

#[test]
fn dragging_past_the_bottom_edge_scrolls_and_extends_the_selection() {
    let text: String = (0..400).map(|i| format!("line {i}\n")).collect();
    let mut h = Harness::new(&text);
    let start = pos2(60.0, 20.0);
    let below = pos2(60.0, SCREEN.bottom() + 40.0);
    let button = |pos, pressed| Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    h.frame(vec![Event::PointerMoved(start), button(start, true)]);
    h.frame(vec![Event::PointerMoved(below)]);
    let first = h.head();
    for _ in 0..30 {
        h.frame(vec![Event::PointerMoved(below)]);
    }
    let later = h.head();
    h.frame(vec![button(below, false)]);
    let line_of = |at: usize| text[..at].matches('\n').count();
    assert!(
        line_of(later) > line_of(first) + 5,
        "holding below the pane keeps scrolling: line {} then {}",
        line_of(first),
        line_of(later)
    );
    assert!(
        h.view.selection().anchor < 20,
        "the selection still starts where it was pressed"
    );
}

#[test]
fn end_on_a_mid_word_wrap_stays_on_the_row() {
    // Review of #28: a row that wraps inside a word used to end before its
    // last character, and the caret at the true end drew on the next row,
    // so a second End walked on.
    let long = "x".repeat(400);
    let mut h = Harness::new(&format!("{long}\nend"));
    h.press(Key::End);
    let row_end = h.head();
    assert!(row_end > 10 && row_end < 400, "wrapped: {row_end}");
    h.press(Key::End);
    assert_eq!(h.head(), row_end, "End again stays on the row");
    // The next row starts exactly there: End reached the row's true end.
    h.press(Key::ArrowRight);
    h.press(Key::Home);
    assert_eq!(h.head(), row_end, "the next row starts where End stopped");
    h.press(Key::ArrowLeft);
    h.press(Key::End);
    h.press(Key::Home);
    assert_eq!(h.head(), 0, "Home from the row's end goes to its start");
    h.press(Key::End);
    h.type_text("Y");
    assert_eq!(
        h.text()[row_end..row_end + 1].to_owned(),
        "Y",
        "typed after the row's last x"
    );
    assert_eq!(h.text().len(), 400 + 1 + 4);
}
