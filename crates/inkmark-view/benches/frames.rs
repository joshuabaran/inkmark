//! Frame benches: both panes driven through egui with synthetic input on
//! the generated fixtures, timing each frame from input to tessellated
//! output (CPU only; no GPU). Reports p50/p95/max and allocations per
//! frame. `cargo bench -p inkmark-view --bench frames [-- <filter>]`, or
//! `scripts/bench.sh` for everything.
//!
//! The screen is a 1600×1000 window; split mode puts the code pane on the
//! left half and the live pane on the right, as the app does.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Event, Id, Key, Modifiers, Pos2, RawInput, Rect, UiBuilder, pos2};
use inkmark_bench::{CountingAlloc, Reporter, allocations, fixtures, iterations};
use inkmark_buffer::{Document, Selection};
use inkmark_parse::{GfmParser, ParseState};
use inkmark_view::{CodeView, FileBrowser, LiveView, ScrollPos};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

/// PLAN.md §5: edit → visible update under 16 ms p95.
const BUDGET: Option<f64> = Some(16.0);
const SCREEN: Rect = Rect::from_min_max(Pos2::ZERO, pos2(1600.0, 1000.0));

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Split,
    Code,
    Live,
}

/// One frame's cost.
#[derive(Clone, Copy, Default)]
struct Cost {
    ms: f64,
    allocs: u64,
}

struct Panes {
    ctx: egui::Context,
    screen: Rect,
    fonts: inkmark_text::SharedFonts,
    code: CodeView,
    live: LiveView,
    doc: Document,
    parse: ParseState,
    mode: Mode,
    time: f64,
    zoom: f32,
}

impl Panes {
    fn new(text: &str, mode: Mode) -> Self {
        Self::with_doc(Document::from_text(text), mode)
    }

    fn open(path: &Path, mode: Mode) -> Self {
        Self::with_doc(Document::open(path).expect("fixture opens"), mode)
    }

    fn with_doc(doc: Document, mode: Mode) -> Self {
        let ctx = egui::Context::default();
        let fonts = inkmark_text::Fonts::shared(&ctx);
        let parse = ParseState::new(Arc::new(GfmParser), &doc, || {});
        let mut p = Self {
            code: CodeView::with_fonts(fonts.clone(), Id::new("code")),
            live: LiveView::with_fonts(fonts.clone(), Id::new("live")),
            fonts,
            ctx,
            doc,
            parse,
            mode,
            screen: SCREEN,
            time: 0.0,
            zoom: 1.0,
        };
        if mode == Mode::Code {
            p.code.request_focus(&p.ctx);
        } else {
            p.live.request_focus(&p.ctx);
        }
        p.settle();
        p
    }

    /// Frames until the full parse has landed, then two more.
    fn settle(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.parse.is_settled() {
            assert!(Instant::now() < deadline, "parse never settled");
            self.frame(vec![]);
            std::thread::sleep(Duration::from_millis(1));
        }
        self.frame(vec![]);
        self.frame(vec![]);
    }

    fn focus_code(&mut self) {
        self.code.request_focus(&self.ctx);
        self.frame(vec![]);
    }

    /// Puts the caret at `offset` in the focused pane and scrolls both
    /// panes to its line.
    fn caret(&mut self, offset: usize) {
        let sel = Selection::caret(offset);
        self.code.set_selection(sel);
        self.live.set_selection(sel);
        let line = self.doc.byte_to_line(offset).saturating_sub(5);
        let pos = ScrollPos { line, frac: 0.0 };
        self.code.set_scroll_pos(pos);
        self.live
            .set_scroll_pos(&self.doc, self.parse.output(), pos);
        self.frame(vec![]);
        self.frame(vec![]);
    }

    fn scroll_to(&mut self, line: usize) {
        let pos = ScrollPos { line, frac: 0.0 };
        self.code.set_scroll_pos(pos);
        self.live
            .set_scroll_pos(&self.doc, self.parse.output(), pos);
    }

    /// One frame: egui pass plus tessellation, as eframe would run it.
    fn frame(&mut self, events: Vec<Event>) -> Cost {
        self.time += 1.0 / 240.0;
        let input = RawInput {
            screen_rect: Some(self.screen),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        self.ctx.set_zoom_factor(self.zoom);
        let (code, live, doc, parse, mode) = (
            &mut self.code,
            &mut self.live,
            &mut self.doc,
            &mut self.parse,
            self.mode,
        );
        let (allocs_before, _) = allocations();
        let started = Instant::now();
        let mut out = self.ctx.run_ui(input, |ui| match mode {
            Mode::Split => {
                let w = ui.max_rect().width() / 2.0;
                let left = Rect::from_min_max(ui.max_rect().min, pos2(w, ui.max_rect().bottom()));
                let right = Rect::from_min_max(pos2(w, ui.max_rect().top()), ui.max_rect().max);
                ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
                    code.show(ui, doc, Some(parse));
                });
                ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
                    live.show(ui, doc, Some(parse));
                });
            }
            Mode::Code => {
                code.show(ui, doc, Some(parse));
            }
            Mode::Live => {
                live.show(ui, doc, Some(parse));
            }
        });
        let shapes = std::mem::take(&mut out.shapes);
        std::hint::black_box(self.ctx.tessellate(shapes, out.pixels_per_point));
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let (allocs_after, _) = allocations();
        out.textures_delta.clear();
        Cost {
            ms,
            allocs: allocs_after - allocs_before,
        }
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) -> Cost {
        self.frame(vec![Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }])
    }

    fn text(&mut self, s: &str) -> Cost {
        self.frame(vec![Event::Text(s.into())])
    }
}

/// Collects frame costs for one bench.
#[derive(Default)]
struct Run {
    ms: Vec<f64>,
    allocs: Vec<u64>,
}

impl Run {
    fn push(&mut self, c: Cost) {
        self.ms.push(c.ms);
        self.allocs.push(c.allocs);
    }

    fn report(&self, r: &Reporter, name: &str, budget: Option<f64>, extra: &[(&str, f64)]) {
        let mut all = extra.to_vec();
        if !self.allocs.is_empty() {
            let mean = self.allocs.iter().sum::<u64>() as f64 / self.allocs.len() as f64;
            all.push(("allocs_per_frame", mean));
        }
        r.record(name, &self.ms, budget, &all);
    }
}

fn ctrl(key: Key) -> (Key, Modifiers) {
    (key, Modifiers::COMMAND)
}

/// A byte offset in the middle of a paragraph around line `line`.
fn mid_paragraph(doc: &Document, mut line: usize) -> usize {
    while doc.line_range(line).len() < 40 || doc.slice(doc.line_range(line)).starts_with('#') {
        line += 1;
    }
    let r = doc.line_range(line);
    let mut at = (r.start + r.end) / 2;
    while !doc.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Byte range of a paragraph whose length is closest to `bytes`, past
/// the middle of `text`.
fn paragraph_near(text: &str, bytes: usize) -> std::ops::Range<usize> {
    let mut best: Option<std::ops::Range<usize>> = None;
    let mut at = 0;
    for p in text.split("\n\n") {
        let r = at..at + p.len();
        at += p.len() + 2;
        if r.start < text.len() / 2 || p.starts_with('#') {
            continue;
        }
        let d = |r: &std::ops::Range<usize>| r.len().abs_diff(bytes);
        if best.as_ref().is_none_or(|b| d(&r) < d(b)) {
            best = Some(r);
        }
        if best.as_ref().is_some_and(|b| b.len() == bytes) {
            break;
        }
    }
    best.expect("a paragraph")
}

fn char_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

fn typing(r: &Reporter, book: &str) {
    let n = iterations(300, 60);
    if r.wants("split/code_typing_mid_5mb") || r.wants("split/code_enter_mid_5mb") {
        let mut p = Panes::new(book, Mode::Split);
        p.focus_code();
        let at = mid_paragraph(&p.doc, p.doc.line_count() / 2);
        p.caret(at);
        let mut run = Run::default();
        for _ in 0..n {
            run.push(p.text("x"));
        }
        run.report(r, "split/code_typing_mid_5mb", BUDGET, &[]);
        let mut run = Run::default();
        for _ in 0..n / 3 {
            run.push(p.key(Key::Enter, Modifiers::NONE));
        }
        run.report(r, "split/code_enter_mid_5mb", BUDGET, &[]);
    }
    for (name, bytes) in [
        ("short", 160usize),
        ("longest", fixtures::LONGEST_PARAGRAPH),
    ] {
        let bench = format!("split/live_typing_{name}_paragraph_5mb");
        if !r.wants(&bench) {
            continue;
        }
        let para = paragraph_near(book, bytes);
        let mut p = Panes::new(book, Mode::Split);
        p.caret(char_boundary(book, (para.start + para.end) / 2));
        let mut run = Run::default();
        for _ in 0..n {
            run.push(p.text("x"));
        }
        run.report(r, &bench, BUDGET, &[("paragraph_bytes", para.len() as f64)]);
    }
    if r.wants("split/live_enter_5mb") {
        let mut p = Panes::new(book, Mode::Split);
        let at = mid_paragraph(&p.doc, p.doc.line_count() / 2);
        p.caret(at);
        let mut run = Run::default();
        for _ in 0..n / 3 {
            run.push(p.key(Key::Enter, Modifiers::NONE));
            run.push(p.text("x"));
        }
        run.report(r, "split/live_enter_5mb", BUDGET, &[]);
    }
}

/// Typing at a human pace: each keystroke, then frames every 4 ms (a
/// 240 Hz display) until the debounced full parse has landed. The frame
/// that sends the parse copies the rope; the one that lands it swaps the
/// output and catches it up.
fn paced(r: &Reporter, book: &str) {
    if !r.wants("split/paced") {
        return;
    }
    let mut p = Panes::new(book, Mode::Split);
    let at = mid_paragraph(&p.doc, p.doc.line_count() / 2);
    p.caret(at);
    let (mut keys, mut landed, mut idle) = (Run::default(), Run::default(), Run::default());
    let mut latency = Vec::new();
    for _ in 0..iterations(40, 10) {
        let started = Instant::now();
        keys.push(p.text("x"));
        let deadline = started + Duration::from_secs(5);
        loop {
            std::thread::sleep(Duration::from_millis(4));
            let was = p.parse.is_settled();
            let c = p.frame(vec![]);
            if !was && p.parse.is_settled() {
                landed.push(c);
                latency.push(started.elapsed().as_secs_f64() * 1000.0);
                break;
            }
            idle.push(c);
            assert!(Instant::now() < deadline, "parse never landed");
        }
    }
    keys.report(r, "split/paced_keystroke_5mb", BUDGET, &[]);
    idle.report(r, "split/paced_wait_frames_5mb", BUDGET, &[]);
    landed.report(r, "split/paced_parse_landed_5mb", BUDGET, &[]);
    r.record(
        "split/paced_keystroke_to_full_parse_5mb",
        &latency,
        None,
        &[],
    );
}

fn scrolling(r: &Reporter, book: &str) {
    let lines = Document::from_text(book).line_count();
    if r.wants("live/scroll_from_top_5mb") {
        let mut p = Panes::new(book, Mode::Live);
        let mut run = Run::default();
        for i in 0..iterations(300, 100) {
            p.scroll_to(i * 3);
            run.push(p.frame(vec![]));
        }
        run.report(r, "live/scroll_from_top_5mb", BUDGET, &[]);
    }
    // The existing bench's scroll (from a third of the way down) and
    // random jumps, kept for comparison with PLAN.md's Results.
    if r.wants("live/scroll_from_third_5mb") || r.wants("live/random_jumps_5mb") {
        let mut p = Panes::new(book, Mode::Live);
        let mut run = Run::default();
        for i in 0..iterations(300, 100) {
            p.scroll_to(lines / 3 + i * 3);
            run.push(p.frame(vec![]));
        }
        run.report(r, "live/scroll_from_third_5mb", BUDGET, &[]);
        let mut seed = 12345u64;
        let mut run = Run::default();
        for _ in 0..iterations(300, 100) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            p.scroll_to((seed % lines as u64) as usize);
            run.push(p.frame(vec![]));
        }
        run.report(r, "live/random_jumps_5mb", BUDGET, &[]);
    }
    // P6: the first jump to the end lays out every block above it.
    if r.wants("jump_end") {
        for (mode, prefix) in [(Mode::Live, "live"), (Mode::Split, "split")] {
            let (mut cold, mut after_edit) = (Run::default(), Run::default());
            for _ in 0..iterations(5, 2) {
                let mut p = Panes::new(book, mode);
                let (k, m) = ctrl(Key::End);
                cold.push(p.key(k, m));
                let (k, m) = ctrl(Key::Home);
                p.key(k, m);
                p.text("x");
                let (k, m) = ctrl(Key::End);
                after_edit.push(p.key(k, m));
            }
            cold.report(r, &format!("{prefix}/jump_end_cold_5mb"), BUDGET, &[]);
            after_edit.report(
                r,
                &format!("{prefix}/jump_end_after_top_edit_5mb"),
                BUDGET,
                &[],
            );
        }
    }
    // The code pane's line cache keeps 4096 lines; scrolling back over
    // lines dropped since re-shapes them.
    if r.wants("code/scroll") {
        let mut p = Panes::new(book, Mode::Code);
        let start = lines / 4;
        let steps = iterations(250, 100);
        let (mut down, mut back) = (Run::default(), Run::default());
        for i in 0..steps {
            p.scroll_to(start + i * 30);
            down.push(p.frame(vec![]));
        }
        for i in (0..steps).rev() {
            p.scroll_to(start + i * 30);
            back.push(p.frame(vec![]));
        }
        down.report(r, "code/scroll_down_5mb", BUDGET, &[]);
        back.report(r, "code/scroll_back_5mb", BUDGET, &[]);
    }
}

/// A frame with no input, as the app's timed repaints draw: split mode,
/// then each pane alone, then split without minimaps.
fn idle(r: &Reporter, book: &str) {
    if !r.wants("idle") {
        return;
    }
    for (name, mode, minimaps) in [
        ("split/idle_frame_5mb", Mode::Split, true),
        ("code/idle_frame_5mb", Mode::Code, true),
        ("live/idle_frame_5mb", Mode::Live, true),
        ("split/idle_frame_no_minimaps_5mb", Mode::Split, false),
    ] {
        let mut p = Panes::new(book, mode);
        p.code.show_minimap = minimaps;
        p.live.show_minimap = minimaps;
        let at = mid_paragraph(&p.doc, p.doc.line_count() / 2);
        p.caret(at);
        let mut run = Run::default();
        for _ in 0..iterations(120, 30) {
            run.push(p.frame(vec![]));
        }
        run.report(r, name, None, &[]);
    }
}

fn tables(r: &Reporter) {
    let text = fixtures::load("tables.md");
    // A cell in the middle of the 500-row table.
    let row = text.lines().nth(250).expect("table row");
    let row_start = text.find(row).unwrap();
    let cell = row_start + row.find("| ").unwrap() + 2;
    if r.wants("live/table_typing_500x8") {
        let mut p = Panes::new(&text, Mode::Split);
        p.caret(cell);
        let mut run = Run::default();
        for _ in 0..iterations(60, 15) {
            run.push(p.text("x"));
        }
        run.report(r, "live/table_typing_500x8", BUDGET, &[]);
        // Leaving the table re-pads it (its own undo step).
        let (k, m) = ctrl(Key::End);
        let mut run = Run::default();
        run.push(p.key(k, m));
        run.push(p.frame(vec![]));
        run.report(r, "live/table_repad_on_leave_500x8", BUDGET, &[]);
    }
    // A column op rewrites every row: Ctrl+Alt+Right inserts a column.
    if r.wants("live/table_insert_column_500x8") {
        let mut p = Panes::new(&text, Mode::Split);
        p.caret(cell);
        let mut run = Run::default();
        for _ in 0..iterations(3, 1) {
            run.push(p.key(Key::ArrowRight, Modifiers::COMMAND | Modifiers::ALT));
            run.push(p.frame(vec![]));
        }
        let columns = p
            .doc
            .slice(p.doc.line_range(p.doc.byte_to_line(cell)))
            .matches(" | ")
            .count()
            + 1;
        run.report(
            r,
            "live/table_insert_column_500x8",
            BUDGET,
            &[("columns_after", columns as f64)],
        );
    }
    // The whole table laid out cold: what the first sight of it costs, and
    // what every cell changing at once (a re-pad) costs again.
    if r.wants("live/table_first_paint_500x8") {
        // Prose first, so opening the note doesn't already draw the table.
        let prose = fixtures::load("prose-1mb.md");
        let cut = prose[..50_000].rfind("\n\n").unwrap();
        let cold = format!("{}\n\n{text}", &prose[..cut]);
        let table_line = cold[..cut + 2 + cell].lines().count();
        let mut run = Run::default();
        for _ in 0..iterations(3, 1) {
            let mut p = Panes::new(&cold, Mode::Live);
            p.scroll_to(table_line);
            run.push(p.frame(vec![]));
        }
        run.report(r, "live/table_first_paint_500x8", BUDGET, &[]);
    }
    if r.wants("code/table_typing_500x8") {
        let mut p = Panes::new(&text, Mode::Split);
        p.focus_code();
        p.caret(cell);
        let mut run = Run::default();
        for _ in 0..iterations(60, 15) {
            run.push(p.text("x"));
        }
        run.report(r, "code/table_typing_500x8", BUDGET, &[]);
    }
}

/// Bytes of RGBA texture egui holds (images, formulas, glyph pages).
fn texture_mb(ctx: &egui::Context) -> f64 {
    let tex = ctx.tex_manager();
    let tex = tex.read();
    tex.allocated()
        .map(|(_, meta)| (meta.size[0] * meta.size[1] * meta.bytes_per_pixel) as f64)
        .sum::<f64>()
        / 1e6
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<f64>().ok())
        .map_or(0.0, |pages| pages * 4096.0 / 1e6)
}

fn images(r: &Reporter) {
    if !r.wants("live/image_note") {
        return;
    }
    let dir = fixtures::dir().join("image-note");
    if !dir.join("images.md").exists() {
        fixtures::write_images(&dir, 24, fixtures::SEED).unwrap();
    }
    let rss_before = rss_mb();
    let mut p = Panes::open(&dir.join("images.md"), Mode::Live);
    let mut run = Run::default();
    // Page through the note. At each screen, keep drawing (every 2 ms)
    // until egui's textures stop changing for 100 ms: the decodes for that
    // screen have landed.
    let lines = p.doc.line_count();
    let mut line = 0;
    while line < lines {
        p.scroll_to(line);
        let (mut last, mut quiet) = (-1.0, 0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while quiet < 50 && Instant::now() < deadline {
            run.push(p.frame(vec![]));
            let now = texture_mb(&p.ctx);
            quiet = if now == last { quiet + 1 } else { 0 };
            last = now;
            std::thread::sleep(Duration::from_millis(2));
        }
        line += 20;
    }
    run.report(
        r,
        "live/image_note_paging",
        BUDGET,
        &[
            ("texture_mb", texture_mb(&p.ctx)),
            ("rss_delta_mb", rss_mb() - rss_before),
        ],
    );
}

/// The image worker's decode for each fixture size, done the way
/// `images.rs` does it: decode, scale down past 4096 px (triangle
/// filter), convert to RGBA for a texture.
fn image_decode(r: &Reporter) {
    if !r.wants("worker/image_decode") {
        return;
    }
    let dir = fixtures::dir().join("image-note").join("images");
    for (i, (w, h)) in fixtures::IMAGE_SIZES.iter().enumerate() {
        let path = dir.join(format!("img-{i}.png"));
        if !path.exists() {
            continue;
        }
        let mut ms = Vec::new();
        for _ in 0..iterations(3, 1) {
            let t = Instant::now();
            let image = image::open(&path).unwrap();
            let image = if image.width() > 4096 || image.height() > 4096 {
                image.resize(4096, 4096, image::imageops::FilterType::Triangle)
            } else {
                image
            };
            let rgba = image.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            std::hint::black_box(egui::ColorImage::from_rgba_unmultiplied(
                size,
                rgba.as_raw(),
            ));
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        r.record(&format!("worker/image_decode_{w}x{h}"), &ms, None, &[]);
    }
}

fn math(r: &Reporter) {
    if !r.wants("live/math") {
        return;
    }
    let text = fixtures::load("math.md");
    let mut first = Run::default();
    let mut paging = Run::default();
    for i in 0..iterations(3, 1) {
        // A fresh view each time: formulas are typeset on first sight.
        let mut p = Panes::new(&text, Mode::Live);
        // `new` already drew the first screen while settling; draw a
        // second document position cold to time a first sight.
        p.scroll_to(40);
        first.push(p.frame(vec![]));
        if i == 0 {
            let lines = p.doc.line_count();
            let mut line = 80;
            while line < lines {
                p.scroll_to(line);
                paging.push(p.frame(vec![]));
                line += 40;
            }
        }
    }
    first.report(r, "live/math_first_sight", BUDGET, &[]);
    paging.report(r, "live/math_paging", BUDGET, &[]);
}

fn long_lines(r: &Reporter) {
    if !r.wants("long_line") {
        return;
    }
    let text = fixtures::load("long-line-1mb.md");
    let at = char_boundary(&text, text.len() / 2);
    for (mode, name) in [(Mode::Code, "code"), (Mode::Live, "live")] {
        let started = Instant::now();
        let mut p = Panes::new(&text, mode);
        let open_ms = started.elapsed().as_secs_f64() * 1000.0;
        p.caret(at);
        let mut run = Run::default();
        for _ in 0..iterations(10, 3) {
            run.push(p.text("x"));
        }
        run.report(
            r,
            &format!("{name}/long_line_1mb_typing"),
            BUDGET,
            &[("open_and_settle_ms", open_ms)],
        );
    }
}

fn huge_list(r: &Reporter) {
    if !r.wants("huge_list") {
        return;
    }
    let text = fixtures::load("huge-list.md");
    let at = text.find("- Item 1000:").unwrap() + 10;
    let mut p = Panes::new(&text, Mode::Split);
    p.caret(at);
    let mut run = Run::default();
    for _ in 0..iterations(100, 20) {
        run.push(p.text("x"));
    }
    run.report(r, "split/live_typing_huge_list", BUDGET, &[]);
}

/// CJK and emoji: scrolling at one zoom, then at several. Every size is
/// a new set of glyphs, and the atlas clears itself when its four pages
/// fill.
fn cjk(r: &Reporter) {
    if !r.wants("cjk") {
        return;
    }
    let text = fixtures::load("cjk-emoji.md");
    for (name, zooms) in [
        ("split/cjk_emoji_scroll", &[1.0f32][..]),
        (
            "split/cjk_emoji_zoom_scroll",
            &[1.0, 1.25, 1.5, 2.0, 3.0, 0.8][..],
        ),
    ] {
        let mut p = Panes::new(&text, Mode::Split);
        let lines = p.doc.line_count();
        let mut run = Run::default();
        for i in 0..iterations(240, 60) {
            p.zoom = zooms[(i / 20) % zooms.len()];
            p.scroll_to((i * 7) % lines);
            run.push(p.frame(vec![]));
        }
        // A renderer on the panes' shared fonts reads their atlas.
        let stats = inkmark_text::TextRenderer::with_fonts(p.fonts.clone()).atlas_stats();
        run.report(
            r,
            name,
            BUDGET,
            &[
                ("atlas_resets", stats.resets as f64),
                ("atlas_pages", stats.pages as f64),
                ("atlas_glyphs", stats.glyphs as f64),
            ],
        );
    }
}

/// Zoom steps (Ctrl+plus/minus, or a scale change): every size is new
/// layout and new glyphs.
fn zoom(r: &Reporter) {
    // The same steps on an empty document: egui's own font atlas and the
    // panes' fixed costs, without any document to re-lay out.
    if r.wants("split/empty_zoom_steps") {
        let mut p = Panes::new("", Mode::Split);
        let mut run = Run::default();
        for i in 0..iterations(30, 10) {
            p.zoom = [1.0, 1.25, 1.5, 2.0, 1.1][i % 5];
            run.push(p.frame(vec![]));
        }
        run.report(r, "split/empty_zoom_steps", BUDGET, &[]);
    }
    if r.wants("split/prose_zoom_steps") {
        let book = fixtures::load("prose-1mb.md");
        let mut p = Panes::new(&book, Mode::Split);
        p.caret(mid_paragraph(&p.doc, p.doc.line_count() / 2));
        let mut run = Run::default();
        for i in 0..iterations(30, 10) {
            p.zoom = [1.0, 1.25, 1.5, 2.0, 1.1][i % 5];
            run.push(p.frame(vec![]));
        }
        run.report(r, "split/prose_zoom_steps_1mb", BUDGET, &[]);
    }
}

/// Dragging the window edge (or the split): the panes get a few points
/// narrower every frame. From 1600 pt wide the live pane stays at its
/// 75-character measure and only the code pane re-wraps; from 1100 pt the
/// live pane is narrower than that measure and re-wraps too.
fn resize(r: &Reporter, book: &str) {
    for (name, text, width) in [
        (
            "split/resize_drag_1mb",
            fixtures::load("prose-1mb.md"),
            1600.0,
        ),
        ("split/resize_drag_5mb", book.to_owned(), 1600.0),
        (
            "split/resize_drag_narrow_1mb",
            fixtures::load("prose-1mb.md"),
            1100.0,
        ),
        ("split/resize_drag_narrow_5mb", book.to_owned(), 1100.0),
    ] {
        if !r.wants(name) {
            continue;
        }
        let mut p = Panes::new(&text, Mode::Split);
        p.screen = Rect::from_min_max(Pos2::ZERO, pos2(width, 1000.0));
        p.caret(mid_paragraph(&p.doc, p.doc.line_count() / 2));
        let mut run = Run::default();
        for i in 0..iterations(20, 6) {
            p.screen = Rect::from_min_max(Pos2::ZERO, pos2(width - 4.0 * (i + 1) as f32, 1000.0));
            run.push(p.frame(vec![]));
        }
        run.report(r, name, BUDGET, &[]);
    }
}

fn sidebar(r: &Reporter) {
    if !r.wants("sidebar") {
        return;
    }
    let root = fixtures::dir().join("folder-10k");
    if !root.exists() {
        fixtures::write_folder(&root, 10_000, fixtures::SEED).unwrap();
    }
    let ctx = egui::Context::default();
    let mut browser = FileBrowser::new(&root);
    let mut time = 0.0;
    let mut frame = |browser: &mut FileBrowser| {
        time += 1.0 / 240.0;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_max(Pos2::ZERO, pos2(300.0, 1000.0))),
            time: Some(time),
            ..Default::default()
        };
        let (a0, _) = allocations();
        let t = Instant::now();
        let mut out = ctx.run_ui(input, |ui| {
            browser.show(ui);
        });
        std::hint::black_box(ctx.tessellate(std::mem::take(&mut out.shapes), 1.0));
        out.textures_delta.clear();
        Cost {
            ms: t.elapsed().as_secs_f64() * 1000.0,
            allocs: allocations().0 - a0,
        }
    };
    let notes = std::fs::read_dir(&root).map_or(0, Iterator::count);
    let deadline = Instant::now() + Duration::from_secs(10);
    let listed = Instant::now();
    while browser.row_count() < notes {
        assert!(Instant::now() < deadline, "listing never finished");
        frame(&mut browser);
        std::thread::sleep(Duration::from_millis(1));
    }
    let list_ms = listed.elapsed().as_secs_f64() * 1000.0;
    let mut run = Run::default();
    for _ in 0..iterations(120, 30) {
        run.push(frame(&mut browser));
    }
    run.report(
        r,
        "sidebar/large_folder_frame",
        BUDGET,
        &[
            ("notes", notes as f64),
            ("listing_ms", list_ms),
            ("rows_painted", browser.painted() as f64),
        ],
    );
}

fn main() {
    let r = Reporter::new("frames");
    let book = fixtures::load("prose-5mb.md");
    typing(&r, &book);
    paced(&r, &book);
    scrolling(&r, &book);
    idle(&r, &book);
    tables(&r);
    images(&r);
    image_decode(&r);
    math(&r);
    long_lines(&r);
    huge_list(&r);
    cjk(&r);
    zoom(&r);
    resize(&r, &book);
    sidebar(&r);
}
