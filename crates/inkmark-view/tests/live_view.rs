//! Drives `LiveView` through egui with synthetic input and a real parse.

use std::sync::Arc;
use std::time::Duration;

use egui::{
    Event, FullOutput, Id, Key, Modifiers, OutputCommand, PointerButton, Pos2, RawInput, Rect, pos2,
};
use inkmark_buffer::{Document, Selection};
use inkmark_parse::{ParseState, PulldownParser};
use inkmark_view::{LiveView, ScrollPos};

const SCREEN: Rect = Rect::from_min_max(Pos2::ZERO, pos2(800.0, 600.0));

struct Harness {
    ctx: egui::Context,
    view: LiveView,
    doc: Document,
    parse: ParseState,
    time: f64,
}

impl Harness {
    fn new(text: &str) -> Self {
        let ctx = egui::Context::default();
        let doc = Document::from_text(text);
        let parse = ParseState::new(Arc::new(PulldownParser), &doc, || {});
        let mut h = Self {
            view: LiveView::new(&ctx, Id::new("live")),
            ctx,
            doc,
            parse,
            time: 0.0,
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
            screen_rect: Some(SCREEN),
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
/// Uses ~/Projects/inkmark/tolstoy.md if present, else synthetic prose.
#[test]
#[ignore]
fn bench_live_scroll_5mb() {
    let book = std::env::var("HOME")
        .ok()
        .and_then(|h| std::fs::read_to_string(format!("{h}/Projects/inkmark/tolstoy.md")).ok())
        .unwrap_or_else(|| {
            "## Chapter\n\nSome *prose* that wraps across a couple of rows in the live view, with a [link](u).\n\n"
                .repeat(50_000)
        });
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
