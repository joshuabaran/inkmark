//! Parse micro benches: full parses, the UI-thread side of a full-parse
//! handoff, and per-keystroke catch-up (rebase plus local reparse).
//! `cargo bench -p inkmark-parse`, or `scripts/bench.sh` for everything.

use std::fmt::Write as _;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use inkmark_bench::fixtures;
use inkmark_buffer::{Document, Edit, EditKind, Selection};
use inkmark_parse::{GfmParser, MarkdownParser, PulldownParser};

fn configure() -> Criterion {
    let c = Criterion::default().configure_from_args();
    if inkmark_bench::quick() {
        c.warm_up_time(Duration::from_millis(200))
            .measurement_time(Duration::from_millis(600))
            .sample_size(10)
    } else {
        c
    }
}

fn full(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse");
    g.sample_size(10);
    for (name, file) in [
        ("prose_5mb", "prose-5mb.md"),
        ("references", "references.md"),
        ("tables", "tables.md"),
        ("huge_list", "huge-list.md"),
    ] {
        let text = fixtures::load(file);
        g.throughput(Throughput::Bytes(text.len() as u64));
        if file == "prose-5mb.md" {
            g.bench_function(format!("commonmark_{name}"), |b| {
                b.iter(|| black_box(PulldownParser.parse(&text)))
            });
        }
        g.bench_function(format!("gfm_{name}"), |b| {
            b.iter(|| black_box(GfmParser.parse(&text)))
        });
    }
    g.finish();
}

/// What the UI thread pays when a debounced full parse starts and lands.
fn handoff(c: &mut Criterion) {
    let text = fixtures::load("prose-5mb.md");
    let doc = Document::from_text(&text);
    let out = GfmParser.parse(&text);
    let mut g = c.benchmark_group("handoff");
    g.sample_size(10);
    g.throughput(Throughput::Bytes(text.len() as u64));
    // `ParseState::update` copies the rope into a String for the worker.
    g.bench_function("rope_to_string_5mb", |b| {
        b.iter(|| black_box(String::from(doc.rope())))
    });
    // `ParseState::accept` drops the output it replaces.
    g.bench_function("drop_output_5mb", |b| {
        b.iter_batched(|| out.clone(), drop, BatchSize::LargeInput)
    });
    g.finish();
}

/// Types `text` at one spot and deletes it again on the next keystroke,
/// so the document (and the edited block) stays the same size however
/// many iterations criterion runs. A growing block would cross the 64 KiB
/// local-reparse limit and stop measuring the reparse.
struct Typist {
    at: usize,
    text: &'static str,
    typed: bool,
}

impl Typist {
    fn new(at: usize, text: &'static str) -> Self {
        Self {
            at,
            text,
            typed: false,
        }
    }

    fn key(&mut self, doc: &mut Document) {
        let (at, len) = (self.at, self.text.len());
        let edit = if self.typed {
            Edit::replace(at..at + len, "")
        } else {
            Edit::insert(at, self.text)
        };
        doc.apply(
            vec![edit],
            Selection::caret(at),
            Selection::caret(at),
            EditKind::Typing,
        )
        .unwrap();
        self.typed = !self.typed;
    }
}

/// A middle offset inside a paragraph of `doc`.
fn mid_paragraph(doc: &Document) -> usize {
    let mut line = doc.line_count() / 2;
    while doc.line_range(line).is_empty() || doc.slice(doc.line_range(line)).starts_with('#') {
        line += 1;
    }
    let r = doc.line_range(line);
    (r.start + r.end) / 2
}

fn catch_up(c: &mut Criterion) {
    let mut g = c.benchmark_group("catch_up");
    for (name, file) in [
        ("prose_5mb", "prose-5mb.md"),
        ("references", "references.md"),
    ] {
        let text = fixtures::load(file);
        let mut doc = Document::from_text(&text);
        let mut out = GfmParser.parse(&text);
        let at = mid_paragraph(&doc);
        // One keystroke: rebase every span after it, re-parse its block.
        let mut typist = Typist::new(at, "x");
        g.bench_function(format!("1_edit_{name}"), |b| {
            b.iter(|| {
                let since = doc.epoch();
                typist.key(&mut doc);
                assert!(out.catch_up(&GfmParser, &doc, since));
            })
        });
        // Enter: same, but the line count changes.
        let mut typist = Typist::new(at, "\n");
        g.bench_function(format!("enter_{name}"), |b| {
            b.iter(|| {
                let since = doc.epoch();
                typist.key(&mut doc);
                assert!(out.catch_up(&GfmParser, &doc, since));
            })
        });
    }
    // 64 edits are the most caught up with local reparses; 65 only shift.
    let text = fixtures::load("prose-5mb.md");
    let mut doc = Document::from_text(&text);
    let mut out = GfmParser.parse(&text);
    let mut typist = Typist::new(mid_paragraph(&doc), "x");
    for n in [64usize, 65] {
        g.bench_function(format!("{n}_edits_prose_5mb"), |b| {
            b.iter(|| {
                let since = doc.epoch();
                for _ in 0..n {
                    typist.key(&mut doc);
                }
                assert!(out.catch_up(&GfmParser, &doc, since));
            })
        });
    }
    g.finish();
}

/// A local reparse of a top-level block of about `size` bytes.
fn local(c: &mut Criterion) {
    let mut g = c.benchmark_group("local_reparse");
    for (name, size) in [("1kb", 1024usize), ("16kb", 16 * 1024), ("63kb", 63 * 1024)] {
        // A tight list is one top-level block of any length.
        let mut list = String::new();
        let mut i = 0;
        while list.len() < size {
            let _ = writeln!(list, "- item {i} with *some* words and a [link](u)");
            i += 1;
        }
        let text = format!("Before.\n\n{list}\nAfter.\n");
        let mut doc = Document::from_text(&text);
        let mut out = GfmParser.parse(&text);
        let mut typist = Typist::new(text.find("item 1 ").unwrap(), "x");
        g.throughput(Throughput::Bytes(list.len() as u64));
        g.bench_function(name, |b| {
            b.iter(|| {
                let since = doc.epoch();
                typist.key(&mut doc);
                assert!(out.catch_up(&GfmParser, &doc, since));
            })
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = configure();
    targets = full, handoff, catch_up, local
}
criterion_main!(benches);
