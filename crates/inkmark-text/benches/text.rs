//! Text micro benches: shaping a paragraph cold and warm, drawing it, and
//! the height cache. The line cache's eviction is measured by scrolling
//! in the frame benches (`inkmark-view/benches/frames.rs`).
//! `cargo bench -p inkmark-text`, or `scripts/bench.sh` for everything.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use egui::Pos2;
use inkmark_bench::fixtures;
use inkmark_text::{GlyphMeshes, HeightCache, RichLine, TextConfig, TextRenderer};

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

/// The live pane's settings, wrapped at a mid-sized panel's width.
const LIVE: TextConfig = TextConfig {
    monospace: false,
    font_size: 16.0,
    line_height: 26.0,
    wrap_width: Some(640.0),
};

/// Unwrapped paragraphs from a fixture near `bytes` long, one per entry.
fn paragraphs_in(file: &str, bytes: usize) -> Vec<String> {
    let book = fixtures::load(file);
    let mut found: Vec<String> = book
        .split("\n\n")
        .filter(|p| !p.starts_with('#'))
        .map(|p| p.replace('\n', " "))
        .filter(|p| p.len() >= bytes * 9 / 10 && p.len() <= bytes * 11 / 10)
        .take(64)
        .collect();
    if found.is_empty() {
        found.push(
            book.split("\n\n")
                .max_by_key(|p| p.len())
                .unwrap()
                .replace('\n', " "),
        );
    }
    found
}

fn paragraphs(bytes: usize) -> Vec<String> {
    paragraphs_in("prose-1mb.md", bytes)
}

fn shaping(c: &mut Criterion) {
    let ctx = egui::Context::default();
    let mut g = c.benchmark_group("shape");
    for (name, bytes) in [
        ("160b", 160usize),
        ("630b", 630),
        ("6900b", fixtures::LONGEST_PARAGRAPH),
    ] {
        let paras = paragraphs(bytes);
        // Cold: text the cache hasn't seen (a counter changes the first
        // characters), so every call shapes and wraps the whole paragraph,
        // as a keystroke in it does.
        let mut text = TextRenderer::new(&ctx);
        text.begin_frame(LIVE, 1.0);
        let mut n = 0u64;
        g.bench_function(format!("cold_{name}"), |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for i in 0..iters {
                    if i % 512 == 0 {
                        // Drop cached lines outside the timing (a width
                        // change clears the cache).
                        text.begin_frame(
                            TextConfig {
                                wrap_width: Some(600.0),
                                ..LIVE
                            },
                            1.0,
                        );
                        text.begin_frame(LIVE, 1.0);
                    }
                    n += 1;
                    let para = &paras[(n as usize) % paras.len()];
                    let line = format!("{n:08} {para}");
                    let t = Instant::now();
                    black_box(text.rich_height(RichLine::plain(&line)));
                    total += t.elapsed();
                }
                total
            })
        });
        // Warm: the same paragraph again. Hash, lookup and geometry rows,
        // which `place` pays for every visible block every frame.
        let line = &paras[0];
        text.rich_height(RichLine::plain(line));
        g.bench_function(format!("warm_geometry_{name}"), |b| {
            b.iter(|| black_box(text.rich_geometry(RichLine::plain(line))))
        });
        g.bench_function(format!("warm_draw_{name}"), |b| {
            b.iter(|| {
                let mut meshes = GlyphMeshes::default();
                text.draw_line(&mut meshes, line, Pos2::ZERO, egui::Color32::WHITE);
                black_box(meshes.vertex_count())
            })
        });
    }
    // Mixed CJK and emoji: each run that the sans font lacks goes through
    // cosmic-text's font fallback.
    let paras = paragraphs_in("cjk-emoji.md", 300);
    let mut text = TextRenderer::new(&ctx);
    text.begin_frame(LIVE, 1.0);
    let mut n = 0u64;
    g.bench_function("cold_cjk_emoji_300b", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                if i % 512 == 0 {
                    text.begin_frame(
                        TextConfig {
                            wrap_width: Some(600.0),
                            ..LIVE
                        },
                        1.0,
                    );
                    text.begin_frame(LIVE, 1.0);
                }
                n += 1;
                let line = format!("{n:08} {}", paras[(n as usize) % paras.len()]);
                let t = Instant::now();
                black_box(text.rich_height(RichLine::plain(&line)));
                total += t.elapsed();
            }
            total
        })
    });
    let paras = paragraphs(300);
    g.bench_function("cold_latin_300b", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for i in 0..iters {
                if i % 512 == 0 {
                    text.begin_frame(
                        TextConfig {
                            wrap_width: Some(600.0),
                            ..LIVE
                        },
                        1.0,
                    );
                    text.begin_frame(LIVE, 1.0);
                }
                n += 1;
                let line = format!("{n:08} {}", paras[(n as usize) % paras.len()]);
                let t = Instant::now();
                black_box(text.rich_height(RichLine::plain(&line)));
                total += t.elapsed();
            }
            total
        })
    });
    g.finish();
}

fn heights(c: &mut Criterion) {
    const LINES: usize = 100_000;
    let mut cache = HeightCache::new((0..LINES).map(|i| 21.0 + (i % 3) as f32));
    let mut g = c.benchmark_group("heights");
    g.bench_function("offset_of_100k", |b| {
        let mut i = 0;
        b.iter(|| {
            i = (i + 7919) % LINES;
            black_box(cache.offset_of(i))
        })
    });
    let total = cache.total();
    g.bench_function("line_at_100k", |b| {
        let mut y = 0.0;
        b.iter(|| {
            y = (y + 7919.5) % total;
            black_box(cache.line_at(y))
        })
    });
    // Typing within a line: same count, O(log n).
    g.bench_function("splice_same_count_100k", |b| {
        b.iter(|| cache.splice(50_000..50_001, [22.0].into_iter()))
    });
    // Enter: one more line, so the Fenwick tree is rebuilt.
    g.bench_function("splice_new_line_100k", |b| {
        b.iter(|| {
            cache.splice(50_000..50_001, [21.0, 21.0].into_iter());
            cache.splice(50_000..50_002, [21.0].into_iter());
        })
    });
    g.finish();
}

criterion_group! {
    name = benches;
    config = configure();
    targets = shaping, heights
}
criterion_main!(benches);
