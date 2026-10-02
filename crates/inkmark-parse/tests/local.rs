//! Rebase + local reparse keep the parse valid after any edit, match a full
//! parse for edits that can't have non-local effects, and the background
//! worker converges to a full parse.

use std::sync::Arc;
use std::time::{Duration, Instant};

use inkmark_buffer::{Document, Edit, EditKind, Selection};
use inkmark_parse::{GfmParser, MarkdownParser, ParseOutput, ParseState, PulldownParser, SpanKind};
use proptest::prelude::*;

const SAMPLE: &str = "# Title *em*\n\nSome **bold** text with `code` and [a link](http://x \"t\").\nSecond line &amp; more.\n\n> quote line\n> - nested *item*\n\n- one\n- two\n\n  continued\n\n```rust\nfn main() {}\n```\n\n    indented code\n\n<div>\nhtml\n</div>\n\n[ref]: /url\n\n---\nLast para\n";

const PIECES: &[&str] = &[
    "a", "é", " ", "\n", "\n\n", "*", "**", "`", "```", "# ", "> ", "- ", "1. ", "[", "](u)", "\\",
    "&amp;", "<b>",
];

fn assert_valid(out: &ParseOutput, doc: &Document) {
    out.map.validate(doc.len()).unwrap();
    let mut prev = 0;
    for b in out.blocks.iter() {
        assert!(
            b.range.end <= doc.len(),
            "block {b:?} past end {}",
            doc.len()
        );
        assert!(
            b.range.start >= prev || b.depth > 0,
            "blocks out of order: {b:?}"
        );
        if b.depth == 0 {
            prev = b.range.start;
        }
    }
}

fn whole(doc: &Document) -> String {
    doc.slice(0..doc.len()).into_owned()
}

fn edit(doc: &mut Document, at: f64, del: usize, insert: &str) -> u64 {
    let before = doc.epoch();
    let text = whole(doc);
    let chars: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let i = ((chars.len() - 1) as f64 * at) as usize;
    let start = chars[i];
    let end = chars[(i + del).min(chars.len() - 1)];
    doc.apply(
        vec![Edit::replace(start..end, insert)],
        Selection::caret(start),
        Selection::caret(start),
        EditKind::Other,
    )
    .unwrap();
    before
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn any_edit_keeps_the_parse_valid(
        edits in prop::collection::vec((0.0..1.0f64, 0usize..4, prop::sample::select(PIECES)), 1..25)
    ) {
        let mut doc = Document::from_text(SAMPLE);
        let mut out = PulldownParser.parse(SAMPLE);
        for (at, del, insert) in edits {
            let since = edit(&mut doc, at, del, insert);
            prop_assert!(out.catch_up(&PulldownParser, &doc, since));
            assert_valid(&out, &doc);
        }
    }

    #[test]
    fn letters_inside_text_match_a_full_parse(picks in prop::collection::vec((0usize..1000, 0usize..1000), 1..20)) {
        let mut doc = Document::from_text(SAMPLE);
        let mut out = PulldownParser.parse(SAMPLE);
        for (span_pick, offset_pick) in picks {
            // A position strictly inside a plain-text span of a paragraph.
            let candidates: Vec<_> = out.map.iter()
                .filter(|s| s.kind == SpanKind::Text && s.range.len() > 2 && s.style == Default::default())
                .map(|s| s.range.clone())
                .collect();
            let range = &candidates[span_pick % candidates.len()];
            let text = whole(&doc);
            let at = range.start + 1 + offset_pick % (range.len() - 1);
            if !text.is_char_boundary(at) || !text[at..].starts_with(|c: char| c.is_alphabetic()) {
                continue;
            }
            let since = doc.epoch();
            doc.apply(vec![Edit::insert(at, "x")], Selection::caret(at), Selection::caret(at + 1), EditKind::Typing).unwrap();
            prop_assert!(out.catch_up(&PulldownParser, &doc, since));
            prop_assert_eq!(&out, &PulldownParser.parse(&whole(&doc)));
        }
    }
}

#[test]
fn opening_a_fence_is_fixed_by_the_full_parse() {
    // Typing ``` before a paragraph turns everything after it into code: a
    // non-local change the local reparse can't see but the worker fixes.
    let mut doc = Document::from_text("para one\n\npara two\n\npara three\n");
    let mut state = ParseState::new(Arc::new(PulldownParser), &doc, || {});
    settle(&mut state, &doc);
    doc.apply(
        vec![Edit::insert(0, "```\n")],
        Selection::caret(0),
        Selection::caret(4),
        EditKind::Other,
    )
    .unwrap();
    state.update(&doc);
    assert_valid(state.output(), &doc);
    assert!(!state.is_settled());
    settle(&mut state, &doc);
    assert_eq!(state.output(), &PulldownParser.parse(&whole(&doc)));
}

#[test]
fn worker_results_from_older_epochs_are_caught_up() {
    let mut doc = Document::from_text(SAMPLE);
    let mut state = ParseState::new(Arc::new(PulldownParser), &doc, || {});
    settle(&mut state, &doc);
    // Edit, let a parse get requested, then keep editing before it lands.
    edit(&mut doc, 0.3, 0, "**new** ");
    std::thread::sleep(inkmark_parse::DEBOUNCE);
    state.update(&doc);
    for i in 0..10 {
        edit(&mut doc, 0.5, 0, if i % 2 == 0 { "a" } else { "\n" });
        state.update(&doc);
        assert_valid(state.output(), &doc);
    }
    settle(&mut state, &doc);
    assert_eq!(state.output(), &PulldownParser.parse(&whole(&doc)));
}

fn settle(state: &mut ParseState, doc: &Document) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.is_settled() {
        assert!(Instant::now() < deadline, "parse never settled");
        state.update(doc);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Parse costs on ~5 MB. Run with:
/// `cargo test --release -p inkmark-parse --test local -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_parse_5mb() {
    let text = SAMPLE.repeat(5_000_000 / SAMPLE.len());
    let start = Instant::now();
    let mut out = PulldownParser.parse(&text);
    let full = start.elapsed();
    let gfm_text = format!("{text}{}", GFM_SAMPLE.repeat(1000));
    let start = Instant::now();
    let gfm = GfmParser.parse(&gfm_text);
    println!(
        "GFM: {:.1} MB with 1000 tables/task lists: full parse {:.0} ms, {} spans",
        gfm_text.len() as f64 / 1e6,
        start.elapsed().as_secs_f64() * 1000.0,
        gfm.map.span_count()
    );

    let mut doc = Document::from_text(&text);
    let mid = doc.line_to_byte(doc.line_count() / 2);
    let mut times = Vec::new();
    for i in 0..200 {
        let since = doc.epoch();
        let at = mid + i;
        let insert = if i % 40 == 39 { "\n" } else { "x" };
        doc.apply(
            vec![Edit::insert(at, insert)],
            Selection::caret(at),
            Selection::caret(at + 1),
            EditKind::Typing,
        )
        .unwrap();
        let t = Instant::now();
        assert!(out.catch_up(&PulldownParser, &doc, since));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    println!(
        "{:.1} MB, {} spans, {} blocks: full parse {:.0} ms; per-keystroke catch-up ms p50={:.2} p95={:.2} max={:.2}",
        text.len() as f64 / 1e6,
        out.map.span_count(),
        out.blocks.len(),
        full.as_secs_f64() * 1000.0,
        times[times.len() / 2],
        times[times.len() * 95 / 100],
        times[times.len() - 1],
    );
}

#[test]
fn local_reparse_picks_up_new_syntax_immediately() {
    let text = "intro\n\nhello world\n\nlast\n".repeat(2000);
    let mut doc = Document::from_text(&text);
    let mut out = PulldownParser.parse(&text);
    let at = doc.len() / 2;
    let at = whole(&doc)[at..].find("hello").unwrap() + at;
    let since = doc.epoch();
    doc.apply(
        vec![Edit::insert(at, "**"), Edit::insert(at + 7, "**")],
        Selection::caret(at),
        Selection::caret(at + 9),
        EditKind::Other,
    )
    .unwrap();
    assert!(out.catch_up(&PulldownParser, &doc, since));
    let bold: Vec<_> = out
        .map
        .spans_in(at..at + 9)
        .into_iter()
        .map(|s| {
            (
                s.range,
                s.kind,
                s.style.contains(inkmark_parse::Style::STRONG),
            )
        })
        .collect();
    assert_eq!(
        bold,
        vec![
            (
                at..at + 2,
                SpanKind::Syntax(inkmark_parse::Syntax::Delimiter),
                true
            ),
            (at + 2..at + 7, SpanKind::Text, true),
            (
                at + 7..at + 9,
                SpanKind::Syntax(inkmark_parse::Syntax::Delimiter),
                true
            ),
        ]
    );
    assert_eq!(out, PulldownParser.parse(&whole(&doc)));
}

const GFM_SAMPLE: &str = "| Name | Done |\n|:-----|-----:|\n| one  | ~~no~~ |\n| two  | yes |\n\n- [ ] task www.example.com/a_b_c\n- [x] done, mail me@example.org\n\nPlain https://x.org/y_z and ~~struck~~ text.\n";

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn gfm_edits_keep_the_parse_valid(
        edits in prop::collection::vec((0.0..1.0f64, 0usize..4, prop::sample::select(&["a", "|", "~~", "[ ] ", "- ", "\n", "www.a.com ", "\\\\", "_"][..])), 1..25)
    ) {
        let mut doc = Document::from_text(GFM_SAMPLE);
        let mut out = GfmParser.parse(GFM_SAMPLE);
        for (at, del, insert) in edits {
            let since = edit(&mut doc, at, del, insert);
            prop_assert!(out.catch_up(&GfmParser, &doc, since));
            assert_valid(&out, &doc);
        }
    }
}

#[test]
fn autolinks_survive_underscore_splits() {
    let src = "see www.example.com/a_b_c now\n";
    let out = GfmParser.parse(src);
    let link: String = out
        .map
        .iter()
        .filter(|s| s.style.contains(inkmark_parse::Style::LINK))
        .map(|s| src[s.range].to_owned())
        .collect();
    assert_eq!(link, "www.example.com/a_b_c");
}

/// Holds the worker inside its first parse until released.
struct Gate {
    calls: std::sync::atomic::AtomicUsize,
    entered: std::sync::Mutex<bool>,
    released: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

impl MarkdownParser for Gate {
    fn parse(&self, src: &str) -> ParseOutput {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            *self.entered.lock().unwrap() = true;
            self.cv.notify_all();
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.cv.wait(released).unwrap();
            }
        }
        GfmParser.parse(src)
    }
}

#[test]
fn a_parse_in_flight_at_reset_is_not_used_for_the_new_document() {
    // Regression for #15.
    let gate = Arc::new(Gate {
        calls: Default::default(),
        entered: std::sync::Mutex::new(false),
        released: std::sync::Mutex::new(false),
        cv: std::sync::Condvar::new(),
    });
    let old = Document::from_text("the old document, still being parsed\n");
    let mut state = ParseState::new(gate.clone(), &old, || {});
    state.update(&old);
    {
        let mut entered = gate.entered.lock().unwrap();
        while !*entered {
            entered = gate.cv.wait(entered).unwrap();
        }
    }
    let new = Document::from_text("# new\n");
    state.reset(&new);
    state.update(&new);
    *gate.released.lock().unwrap() = true;
    gate.cv.notify_all();
    settle(&mut state, &new);
    assert_eq!(state.output().map.len(), new.len());
    assert_eq!(state.output(), &GfmParser.parse("# new\n"));
}
