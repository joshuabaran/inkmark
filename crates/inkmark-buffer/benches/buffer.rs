//! Buffer micro benches: edits, undo/redo, find, open and save on the
//! generated 5 MB book (`inkmark-bench` fixtures). `cargo bench -p
//! inkmark-buffer`, or `scripts/bench.sh` for everything.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use inkmark_bench::fixtures;
use inkmark_buffer::find::{Finder, SearchOptions};
use inkmark_buffer::{Document, Edit, EditKind, Selection};

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

fn edits(c: &mut Criterion) {
    let book = fixtures::load("prose-5mb.md");
    let mut g = c.benchmark_group("buffer");
    // One typed character in the middle of 5 MB, as a keystroke does.
    let mut doc = Document::from_text(&book);
    let mid = doc.line_to_byte(doc.line_count() / 2);
    let mut at = mid;
    g.bench_function("apply_typed_char_5mb", |b| {
        b.iter(|| {
            doc.apply(
                vec![Edit::insert(at, "x")],
                Selection::caret(at),
                Selection::caret(at + 1),
                EditKind::Typing,
            )
            .unwrap();
            at += 1;
        })
    });
    // Undo then redo of a sealed one-character step.
    let mut doc = Document::from_text(&book);
    doc.apply(
        vec![Edit::insert(mid, "x")],
        Selection::caret(mid),
        Selection::caret(mid + 1),
        EditKind::Other,
    )
    .unwrap();
    doc.seal_undo_step();
    g.bench_function("undo_redo_5mb", |b| {
        b.iter(|| {
            black_box(doc.undo());
            black_box(doc.redo());
        })
    });
    g.finish();
}

fn find(c: &mut Criterion) {
    let book = fixtures::load("prose-5mb.md");
    let doc = Document::from_text(&book);
    let rope = doc.rope();
    let mid = doc.len() / 2;
    let plain = SearchOptions {
        case_sensitive: false,
        regex: false,
    };
    let regex = SearchOptions {
        case_sensitive: true,
        regex: true,
    };
    let mut g = c.benchmark_group("find");
    g.throughput(Throughput::Bytes(doc.len() as u64));
    g.sample_size(20);
    // What the find bar does on every change to the query: compile, then
    // one pass that lists (up to its limit) and counts every match.
    g.bench_function("query_common_word_5mb", |b| {
        b.iter(|| {
            let f = Finder::compile("the", plain).unwrap();
            black_box(f.collect_matches(rope, 10_000, mid))
        })
    });
    g.bench_function("query_rare_word_5mb", |b| {
        b.iter(|| {
            let f = Finder::compile("Kutúzov", plain).unwrap();
            black_box(f.collect_matches(rope, 10_000, mid))
        })
    });
    g.bench_function("query_regex_5mb", |b| {
        b.iter(|| {
            let f = Finder::compile(r"\b\w+ly\b", regex).unwrap();
            black_box(f.collect_matches(rope, 10_000, mid))
        })
    });
    let common = Finder::compile("the", plain).unwrap();
    g.bench_function("census_common_word_5mb", |b| {
        b.iter(|| black_box(common.census(rope, None)))
    });
    // Shift+F3 past the stored list's limit walks from the start.
    g.bench_function("prev_from_middle_5mb", |b| {
        b.iter(|| black_box(common.prev(rope, mid)))
    });
    g.throughput(Throughput::Elements(1));
    g.bench_function("next_from_middle_5mb", |b| {
        b.iter(|| black_box(common.next(rope, mid)))
    });
    g.finish();

    let small = fixtures::load("prose-1mb.md");
    let mut g = c.benchmark_group("find");
    g.sample_size(10);
    g.bench_function("replace_all_common_word_1mb", |b| {
        b.iter_batched(
            || Document::from_text(&small),
            |mut doc| {
                let f = Finder::compile("the", plain).unwrap();
                black_box(f.replace_all(&mut doc, "THE", Selection::caret(0)))
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

fn files(c: &mut Criterion) {
    // Under target/, so the save hits the real disk (/tmp is often tmpfs).
    let target = fixtures::workspace_root().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let dir = tempfile::tempdir_in(&target).unwrap();
    let mut g = c.benchmark_group("file");
    g.sample_size(10);
    for (name, file) in [
        ("1mb", "prose-1mb.md"),
        ("5mb", "prose-5mb.md"),
        ("10mb", "prose-10mb.md"),
    ] {
        let text = fixtures::load(file);
        let path = dir.path().join(file);
        std::fs::write(&path, &text).unwrap();
        g.throughput(Throughput::Bytes(text.len() as u64));
        // Read (page cache), UTF-8 check, CRLF/BOM handling, rope build.
        g.bench_function(format!("open_{name}"), |b| {
            b.iter(|| black_box(Document::open(&path).unwrap()))
        });
        // Encode, write, fsync, rename and directory fsync on the disk
        // under the temp dir.
        let mut doc = Document::open(&path).unwrap();
        g.bench_function(format!("save_{name}"), |b| b.iter(|| doc.save().unwrap()));
        // The once-a-second disk check.
        g.throughput(Throughput::Elements(1));
        g.bench_function(format!("disk_status_{name}"), |b| {
            b.iter(|| black_box(doc.disk_status().unwrap()))
        });
    }
    g.finish();
}

criterion_group! {
    name = benches;
    config = configure();
    targets = edits, find, files
}
criterion_main!(benches);
