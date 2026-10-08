//! Drives `LiveView` through egui with synthetic input and a real parse.

use std::sync::Arc;
use std::time::Duration;

use egui::{
    Event, FullOutput, Id, Key, Modifiers, MouseWheelUnit, OutputCommand, PointerButton, Pos2,
    RawInput, Rect, TouchPhase, pos2, vec2,
};
use inkmark_buffer::{Document, Edit, EditKind, Selection};
use inkmark_parse::{ParseState, PulldownParser};
use inkmark_view::{LiveView, ScrollPos};

const SCREEN: Rect = Rect::from_min_max(Pos2::ZERO, pos2(800.0, 600.0));

struct Harness {
    ctx: egui::Context,
    view: LiveView,
    doc: Document,
    parse: ParseState,
    time: f64,
    screen: Rect,
}

impl Harness {
    fn new(text: &str) -> Self {
        Self::open(text, SCREEN)
    }

    fn open(text: &str, screen: Rect) -> Self {
        let ctx = egui::Context::default();
        let doc = Document::from_text(text);
        let parse = ParseState::new(Arc::new(PulldownParser), &doc, || {});
        let mut h = Self {
            view: LiveView::new(&ctx, Id::new("live")),
            ctx,
            doc,
            parse,
            time: 0.0,
            screen,
        };
        h.view.request_focus(&h.ctx);
        while !h.parse.is_settled() {
            h.frame(vec![]);
            std::thread::sleep(Duration::from_millis(2));
        }
        h.frame(vec![]);
        h.frame(vec![]);
        h
    }

    fn frame(&mut self, events: Vec<Event>) -> FullOutput {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(self.screen),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        let (view, doc, parse) = (&mut self.view, &mut self.doc, &mut self.parse);
        let mut out = self.ctx.run_ui(input, |ui| {
            view.show(ui, doc, Some(parse));
        });
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

    fn head(&self) -> usize {
        self.view.selection().head
    }
}

#[test]
fn arrows_skip_hidden_container_prefixes() {
    let src = "> - one\n>   two\n";
    let mut h = Harness::new(src);
    let end_of_one = src.find("one").unwrap() + 3;
    h.view.set_selection(Selection::caret(end_of_one));
    h.frame(vec![]);
    // The newline shows as a space, and line two starts with ">   ",
    // drawn as a bar and indent: one step lands on "two".
    h.press(Key::ArrowRight);
    assert_eq!(h.head(), src.find("two").unwrap());
    h.press(Key::ArrowLeft);
    assert_eq!(h.head(), end_of_one);
}

#[test]
fn up_and_down_walk_blocks_and_blank_lines() {
    let src = "# Title\n\nfirst para\n\nsecond para\n";
    let mut h = Harness::new(src);
    let mut stops = vec![h.head()];
    for _ in 0..4 {
        h.press(Key::ArrowDown);
        stops.push(h.head());
    }
    assert!(
        stops.windows(2).all(|w| w[0] < w[1]),
        "monotonic: {stops:?}"
    );
    assert_eq!(*stops.last().unwrap(), src.find("second").unwrap());
    for _ in 0..4 {
        h.press(Key::ArrowUp);
    }
    // Arriving from below, the heading is rendered without its "# ", so
    // the leftmost spot is the start of the title.
    assert_eq!(h.head(), src.find("Title").unwrap());
}

#[test]
fn clicks_map_through_hidden_syntax() {
    let src = "# Title\n\nSome **bold** text\n";
    let mut h = Harness::new(src);
    // Put the caret elsewhere so the heading is rendered, not revealed.
    h.key(Key::End, Modifiers::COMMAND);
    // Far right of the heading row: the end of "Title".
    h.click(pos2(700.0, 30.0));
    assert_eq!(h.head(), src.find("Title").unwrap() + 5);
    // Far left of the paragraph: its first character.
    h.key(Key::End, Modifiers::COMMAND);
    h.click(pos2(30.0, 110.0));
    assert_eq!(h.head(), src.find("Some").unwrap());
}

#[test]
fn copy_gives_markdown_source() {
    let src = "Some **bold** text\n";
    let mut h = Harness::new(src);
    h.key(Key::A, Modifiers::COMMAND);
    let out = h.frame(vec![Event::Copy]);
    assert!(
        out.platform_output
            .commands
            .contains(&OutputCommand::CopyText(src.into()))
    );
}

#[test]
fn scroll_position_round_trips() {
    let para = "A paragraph of prose that wraps over a few rows in the live view, \
                with *some emphasis* and a [link](http://x) to make it realistic.\n\n";
    let mut h = Harness::new(&para.repeat(2000));
    for line in [0, 500, 1301, 3998] {
        let parse = h.parse.output().clone();
        h.view
            .set_scroll_pos(&h.doc, &parse, ScrollPos { line, frac: 0.0 });
        // Heights get measured as blocks are drawn; give it a frame to settle.
        h.frame(vec![]);
        h.view
            .set_scroll_pos(&h.doc, &parse, ScrollPos { line, frac: 0.0 });
        h.frame(vec![]);
        let got = h.view.scroll_pos(&h.doc, &parse);
        assert!(
            got.line.abs_diff(line) <= 1,
            "asked for {line}, got {got:?}"
        );
    }
}

/// Live view frame cost on a ~5 MB book. Run with:
/// `cargo test --release -p inkmark-view --test live_view -- --ignored --nocapture`
/// Uses the generated 5 MB book (`inkmark-bench` fixtures), the same on every machine.
#[test]
#[ignore]
fn bench_live_scroll_5mb() {
    let book = inkmark_bench::fixtures::load("prose-5mb.md");
    let mut h = Harness::new(&book);
    let lines = h.doc.line_count();
    let parse = h.parse.output().clone();
    let run = |h: &mut Harness, next: &mut dyn FnMut(usize) -> usize| {
        let mut times = Vec::new();
        for i in 0..300 {
            let line = next(i);
            h.view
                .set_scroll_pos(&h.doc, &parse, ScrollPos { line, frac: 0.0 });
            let t = std::time::Instant::now();
            h.frame(vec![]);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        (
            times[times.len() / 2],
            times[times.len() * 95 / 100],
            times[times.len() - 1],
        )
    };
    let start = lines / 3;
    let scroll = run(&mut h, &mut |i| start + i * 3);
    let mut seed = 12345u64;
    let jump = run(&mut h, &mut |_| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % lines as u64) as usize
    });
    println!(
        "{:.1} MB, {lines} lines. live frame ms: scroll p50={:.2} p95={:.2} max={:.2}; jump p50={:.2} p95={:.2} max={:.2}",
        h.doc.len() as f64 / 1e6,
        scroll.0,
        scroll.1,
        scroll.2,
        jump.0,
        jump.1,
        jump.2
    );
}

#[test]
fn image_paragraphs_show_the_loaded_image() {
    let dir = tempfile::tempdir().unwrap();
    image::RgbaImage::from_pixel(100, 300, image::Rgba([0, 128, 255, 255]))
        .save(dir.path().join("tall.png"))
        .unwrap();
    let path = dir.path().join("doc.md");
    std::fs::write(&path, "Intro\n\n![tall](tall.png)\n\nOutro\n").unwrap();

    let ctx = egui::Context::default();
    let doc = Document::open(&path).unwrap();
    let parse = ParseState::new(Arc::new(PulldownParser), &doc, || {});
    let mut h = Harness {
        view: LiveView::new(&ctx, Id::new("live")),
        ctx,
        doc,
        parse,
        time: 0.0,
        screen: SCREEN,
    };
    // Caret on "Intro", so the image paragraph shows only the image.
    h.view.request_focus(&h.ctx);
    let mut height = 0.0;
    for _ in 0..500 {
        h.frame(vec![]);
        height = h.view.content_height();
        if height > 300.0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        height > 300.0,
        "image never loaded: content height {height}"
    );
    // With the caret on the image's line, its source shows above the image.
    let at = h.doc.slice(0..h.doc.len()).find("![").unwrap();
    h.view.set_selection(Selection::caret(at));
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(
        h.view.content_height() > height + 10.0,
        "revealed source adds a row"
    );
}

#[test]
fn live_minimap_click_jumps() {
    let para = "A paragraph of prose that wraps over a couple of rows in the live view.\n\n";
    let mut h = Harness::new(&format!("# Start\n\n{}", para.repeat(3000)));
    let x = 800.0 - 10.0 - inkmark_minimap::WIDTH / 2.0;
    let press = |pressed| Event::PointerButton {
        pos: pos2(x, 590.0),
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    h.frame(vec![Event::PointerMoved(pos2(x, 590.0)), press(true)]);
    h.frame(vec![press(false)]);
    let parse = h.parse.output().clone();
    let line = h.view.scroll_pos(&h.doc, &parse).line;
    assert!(line > 100, "jumped to line {line}");
}

#[test]
fn live_minimap_drag_tracks() {
    let para = "A paragraph of prose that wraps over a couple of rows in the live view.\n\n";
    let mut h = Harness::new(&format!("# Start\n\n{}", para.repeat(3000)));
    let x = 800.0 - 10.0 - inkmark_minimap::WIDTH / 2.0;
    let button = |y: f32, pressed| Event::PointerButton {
        pos: pos2(x, y),
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    // A press near the top jumps a short way. Later moves, with the button
    // still down, have to keep scrolling. The minimap is outside the text.
    h.frame(vec![
        Event::PointerMoved(pos2(x, 120.0)),
        button(120.0, true),
    ]);
    let parse = h.parse.output().clone();
    let pressed_at = h.view.scroll_pos(&h.doc, &parse).line;
    for y in [240.0, 400.0, 590.0] {
        h.frame(vec![Event::PointerMoved(pos2(x, y))]);
    }
    let dragged = h.view.scroll_pos(&h.doc, &parse).line;
    assert!(
        dragged > pressed_at + 50,
        "press landed on {pressed_at}, drag stayed at {dragged}"
    );
}

/// A click in the text column, past the end of the first visual row.
fn first_row_end(screen: Rect) -> usize {
    let src = "word ".repeat(100);
    let mut h = Harness::open(&src, screen);
    let x = screen.right() - 10.0 - inkmark_minimap::WIDTH - 8.0;
    h.click(pos2(x, 8.0));
    h.head()
}

#[test]
fn the_live_pane_wraps_near_seventy_five_characters() {
    let wide = first_row_end(Rect::from_min_max(Pos2::ZERO, pos2(1800.0, 900.0)));
    let medium = first_row_end(Rect::from_min_max(Pos2::ZERO, pos2(1000.0, 700.0)));
    let narrow = first_row_end(Rect::from_min_max(Pos2::ZERO, pos2(480.0, 600.0)));
    assert!(
        (40..=110).contains(&wide) && (40..=110).contains(&medium),
        "wide {wide}, medium {medium}"
    );
    assert!(
        wide.abs_diff(medium) <= 15,
        "wide {wide} and medium {medium} diverged"
    );
    assert!(
        narrow + 20 <= wide.min(medium),
        "narrow {narrow}, wide {wide}, medium {medium}"
    );
}

#[test]
fn an_edit_inside_a_block_replaces_its_height() {
    let block = "\
The first line of the paragraph stays measured above the viewport.
The second line belongs to that same paragraph.
The third line is where the edit lands.
";
    let src = format!("{block}\n{}", "Hello.\n\n".repeat(200));
    let mut h = Harness::new(&src);
    let parse = h.parse.output().clone();
    h.view.set_scroll_pos(
        &h.doc,
        &parse,
        ScrollPos {
            line: 60,
            frac: 0.0,
        },
    );
    h.frame(vec![]);
    let parse = h.parse.output().clone();
    let scrolled = h.view.scroll_pos(&h.doc, &parse).line;
    assert!(scrolled > 20, "scrolled to line {scrolled}");
    assert!(h.view.line_measured(0), "the paragraph was measured");
    assert_eq!(
        h.view.measured_height(1),
        0.0,
        "the block's height sits on its first line"
    );
    let before = h.view.measured_height(0);

    let at = src.find("edit lands").unwrap();
    let sel = h.view.selection();
    h.doc
        .apply(
            vec![Edit::insert(at, "XX")],
            sel,
            Selection::caret(at + 2),
            EditKind::Typing,
        )
        .unwrap();
    h.frame(vec![]);
    let parse = h.parse.output().clone();
    let still = h.view.scroll_pos(&h.doc, &parse).line;
    assert!(still > 20, "the edit pulled the view to line {still}");
    let after = h.view.measured_height(0);
    let edited = format!("{}XX{}", &block[..at], &block[at..]);
    let fresh = Harness::new(&edited);
    let expected = fresh.view.measured_height(0);
    assert!(
        (after - expected).abs() < 1.0,
        "stored {after}, one layout {expected}, before the edit {before}"
    );
}

#[test]
fn blocks_below_the_viewport_stay_estimated() {
    let src = "Hello.\n\n".repeat(500);
    let h = Harness::new(&src);
    assert!(h.view.line_measured(0), "the first line is on screen");
    assert!(
        !h.view.line_measured(400),
        "a block below the viewport stays estimated"
    );
}

#[test]
fn a_wheel_holds_its_place_on_the_next_frame() {
    let para = "A paragraph of prose that wraps over a few rows in the live view, \
                with *some emphasis* and a [link](http://x) to make it realistic.\n\n";
    let mut h = Harness::new(&para.repeat(400));
    let mut events = vec![Event::PointerMoved(pos2(80.0, 80.0))];
    for _ in 0..40 {
        events.push(Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: vec2(0.0, -7.0),
            phase: TouchPhase::Move,
            modifiers: Modifiers::NONE,
        });
    }
    h.frame(events);
    let parse = h.parse.output().clone();
    let after = h.view.scroll_pos(&h.doc, &parse);
    h.frame(vec![]);
    let settled = h.view.scroll_pos(&h.doc, &parse);
    assert_eq!(after.line, settled.line, "wheel {after:?} then {settled:?}");
    assert!(
        (after.frac - settled.frac).abs() < 0.02,
        "wheel {after:?} then {settled:?}"
    );
    assert!(after.line > 0, "the wheel moved down to {after:?}");
    let below = (after.line + 80).min(h.doc.line_count().saturating_sub(1));
    assert!(
        below > after.line && !h.view.line_measured(below),
        "line {below} is below the viewport and stays estimated"
    );
}

/// Headings and wrapped paragraphs: blocks whose estimated heights differ
/// from their laid-out ones, so any estimate that leaked into a scroll
/// would show.
fn parts(n: usize) -> String {
    (0..n)
        .map(|i| {
            format!(
                "## Part {i}\n\nA paragraph of prose that wraps over a few rows in the live \
                 view, with *some emphasis* and a [link](http://x) to make it realistic, \
                 number {i}.\n\n"
            )
        })
        .collect()
}

fn line_of(src: &str, needle: &str) -> usize {
    src[..src.find(needle).unwrap()].matches('\n').count()
}

#[test]
fn a_cold_jump_to_the_end_lays_out_only_the_screen() {
    let src = parts(400);
    let mut h = Harness::new(&src);
    h.key(Key::End, Modifiers::COMMAND);
    h.frame(vec![]);
    assert_eq!(h.head(), h.doc.len());
    let (top, _) = h.view.view_top();
    let lines = h.doc.line_count();
    assert!(
        top + 60 > lines,
        "the view is at the end: top {top} of {lines}"
    );
    assert!(h.view.line_measured(top), "the screen is measured");
    let middle = line_of(&src, "## Part 200");
    assert!(
        !h.view.line_measured(middle),
        "a block far above the jump stays estimated"
    );
}

#[test]
fn a_wheel_up_after_a_cold_jump_moves_exactly_its_distance() {
    let mut h = Harness::new(&parts(400));
    h.key(Key::End, Modifiers::COMMAND);
    h.frame(vec![]);
    let (line0, off0) = h.view.view_top();
    h.frame(vec![
        Event::PointerMoved(pos2(80.0, 80.0)),
        Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: vec2(0.0, 300.0),
            phase: TouchPhase::Move,
            modifiers: Modifiers::NONE,
        },
    ]);
    // egui spreads a wheel over a few frames.
    for _ in 0..120 {
        h.frame(vec![]);
    }
    let (line1, off1) = h.view.view_top();
    assert!(line1 < line0, "moved up from {line0} to {line1}");
    // Everything the scroll passed through was laid out before it moved
    // the view, so the distance is exact rather than an estimate.
    let mut between = 0.0;
    for l in line1..line0 {
        assert!(
            h.view.line_measured(l),
            "line {l} was scrolled past unmeasured"
        );
        between += h.view.measured_height(l);
    }
    let moved = between - off1 + off0;
    assert!(
        (moved - 300.0).abs() < 1.5,
        "moved {moved} for a 300 pt wheel"
    );
}

#[test]
fn page_up_after_a_cold_jump_measures_what_it_crosses() {
    let mut h = Harness::new(&parts(400));
    h.key(Key::End, Modifiers::COMMAND);
    h.frame(vec![]);
    let from = h.doc.byte_to_line(h.head());
    h.press(Key::PageUp);
    h.frame(vec![]);
    let to = h.doc.byte_to_line(h.head());
    assert!(to + 5 < from, "Page Up moved the caret from {from} to {to}");
    for l in to..from {
        assert!(h.view.line_measured(l), "line {l} was crossed unmeasured");
    }
    let (top, _) = h.view.view_top();
    assert!(top <= to, "the caret on line {to} is in view from {top}");
}

#[test]
fn a_wrap_change_keeps_the_view_and_lays_out_only_the_screen() {
    let src = parts(400);
    let wide = Rect::from_min_max(Pos2::ZERO, pos2(1400.0, 700.0));
    let mut h = Harness::open(&src, wide);
    let middle = line_of(&src, "## Part 300");
    let parse = h.parse.output().clone();
    h.view.set_scroll_pos(
        &h.doc,
        &parse,
        ScrollPos {
            line: middle,
            frac: 0.0,
        },
    );
    h.frame(vec![]);
    let before = h.view.scroll_pos(&h.doc, &parse);
    // Narrower than the live pane's 75-character measure: every line wraps
    // differently and every height is estimated again.
    h.screen = Rect::from_min_max(Pos2::ZERO, pos2(500.0, 700.0));
    h.frame(vec![]);
    h.frame(vec![]);
    let after = h.view.scroll_pos(&h.doc, &parse);
    assert_eq!(before.line, after.line, "{before:?} then {after:?}");
    assert!(
        h.view.line_measured(middle),
        "the screen was laid out again"
    );
    let far = line_of(&src, "## Part 100");
    assert!(
        !h.view.line_measured(far),
        "a block far above the view stays estimated after the change"
    );
}

#[test]
fn an_edit_far_above_the_view_leaves_it_in_place() {
    let src = parts(400);
    let mut h = Harness::new(&src);
    let middle = line_of(&src, "## Part 300");
    let parse = h.parse.output().clone();
    h.view.set_scroll_pos(
        &h.doc,
        &parse,
        ScrollPos {
            line: middle,
            frac: 0.0,
        },
    );
    h.frame(vec![]);
    let (top, offset) = h.view.view_top();
    // The first paragraph was on screen at open. An edit there that adds
    // two lines shifts the view by exactly those lines and nothing else:
    // the view is anchored to its line, not to a height.
    let at = src.find("number 0.").unwrap();
    let sel = h.view.selection();
    h.doc
        .apply(
            vec![Edit::insert(
                at,
                "a longer ending that wraps onto another row.\n\nNew ",
            )],
            sel,
            sel,
            EditKind::Other,
        )
        .unwrap();
    h.frame(vec![]);
    assert_eq!(h.view.view_top(), (top + 2, offset));
}

/// A paragraph of wide letters over many source lines. Its estimate
/// (characters × the average advance) under-counts its rows, so its real
/// height is well past what the height cache holds until it's laid out.
fn wide_paragraph(words: usize) -> String {
    (0..words)
        .map(|i| if i % 9 == 8 { "WWWWWW\n" } else { "WWWWWW " })
        .collect::<String>()
        .trim_end()
        .to_owned()
        + "\n"
}

/// Where `key` takes the caret from `at`, when the caret's block was
/// measured before a zoom and the view has moved far away since, so the
/// zoom left that block estimated.
fn key_after_rebuild(src: &str, at: usize, key: Key) -> usize {
    let mut h = Harness::new(src);
    // Placed without scrolling to it, as when the other pane hands the
    // caret over.
    h.view.mirror_selection(Selection::caret(at));
    let parse = h.parse.output().clone();
    let far = h.doc.line_count() - 5;
    h.view.set_scroll_pos(
        &h.doc,
        &parse,
        ScrollPos {
            line: far,
            frac: 0.0,
        },
    );
    h.frame(vec![]);
    h.ctx.set_zoom_factor(1.25);
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(
        !h.view.line_measured(0),
        "the zoom left the caret's block estimated"
    );
    h.press(key);
    h.head()
}

/// The same move with the caret's block on screen and laid out.
fn key_on_screen(src: &str, at: usize, key: Key) -> usize {
    let mut h = Harness::new(src);
    h.ctx.set_zoom_factor(1.25);
    h.frame(vec![]);
    h.view.set_selection(Selection::caret(at));
    h.frame(vec![]);
    assert!(h.view.line_measured(0), "the caret's block is on screen");
    h.press(key);
    h.head()
}

#[test]
fn arrow_up_in_an_estimated_block_stays_on_the_real_row() {
    let wide = wide_paragraph(300);
    let src = format!("{wide}\n{}", "Filler.\n\n".repeat(300));
    let at = wide.len() - 10;
    let expected = key_on_screen(&src, at, Key::ArrowUp);
    assert!(
        expected < at && expected > at - 120,
        "one row up: {expected} from {at}"
    );
    assert_eq!(key_after_rebuild(&src, at, Key::ArrowUp), expected);
}

#[test]
fn page_up_in_a_block_taller_than_the_view_stays_on_the_real_row() {
    let wide = wide_paragraph(700);
    let src = format!("{wide}\n{}", "Filler.\n\n".repeat(300));
    let at = wide.len() - 10;
    let expected = key_on_screen(&src, at, Key::PageUp);
    assert!(
        expected > 0 && expected < at,
        "a page up inside the block: {expected}"
    );
    assert_eq!(key_after_rebuild(&src, at, Key::PageUp), expected);
}
