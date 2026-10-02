//! M1 glyph-atlas spike: scroll 100k lines through cosmic-text → swash → egui Mesh.
//!
//! ```text
//! cargo run --release -p inkmark-text --example atlas_spike -- [FILE] [--lines N]
//!     [--bench SECS] [--speed PTS_PER_SEC] [--jump] [--proportional] [--no-wrap] [--goto LINE] [--scale ZOOM]
//!     [--glow]  (needs `--features glow`)
//! ```
//!
//! Without FILE, generates N (default 100k) synthetic lines with mixed lengths,
//! accents, CJK and emoji. `--bench` autoscrolls (or jumps to random lines
//! every frame with `--jump`) for SECS seconds, prints frame stats and exits.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Rect, Sense, pos2, vec2};
use inkmark_text::{GlyphMeshes, HeightCache, ScrollAnchor, TextConfig, TextRenderer};

const SCROLLBAR_WIDTH: f32 = 12.0;
const PADDING: f32 = 8.0;
const STATS_WINDOW: usize = 240;

struct Args {
    file: Option<String>,
    lines: usize,
    goto: usize,
    scale: f32,
    glow: bool,
    bench: Option<f32>,
    speed: f32,
    jump: bool,
    monospace: bool,
    wrap: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        file: None,
        lines: 100_000,
        goto: 0,
        scale: 1.0,
        glow: false,
        bench: None,
        speed: 2000.0,
        jump: false,
        monospace: true,
        wrap: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| -> f32 {
            it.next()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("{name} needs a number"))
        };
        match arg.as_str() {
            "--lines" => args.lines = value("--lines") as usize,
            "--goto" => args.goto = value("--goto") as usize,
            "--scale" => args.scale = value("--scale"),
            "--bench" => args.bench = Some(value("--bench")),
            "--speed" => args.speed = value("--speed"),
            "--jump" => args.jump = true,
            "--glow" => args.glow = true,
            "--proportional" => args.monospace = false,
            "--no-wrap" => args.wrap = false,
            _ if arg.starts_with("--") => panic!("unknown flag {arg}"),
            _ => args.file = Some(arg),
        }
    }
    args
}

fn main() -> eframe::Result {
    let args = parse_args();
    let lines = match &args.file {
        Some(path) => std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("reading {path}: {e}"))
            .lines()
            .map(str::to_owned)
            .collect(),
        None => synthetic_lines(args.lines),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("inkmark atlas spike")
            .with_app_id("inkmark")
            .with_inner_size([1200.0, 900.0]),
        ..Default::default()
    };
    #[cfg(feature = "glow")]
    let options = eframe::NativeOptions {
        renderer: if args.glow {
            eframe::Renderer::Glow
        } else {
            eframe::Renderer::Wgpu
        },
        ..options
    };
    #[cfg(not(feature = "glow"))]
    assert!(!args.glow, "--glow needs `--features glow`");
    eframe::run_native(
        "inkmark atlas spike",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::Theme::Dark);
            // Stand-in for monitor scaling: same pixels_per_point path.
            cc.egui_ctx.set_zoom_factor(args.scale);
            Ok(Box::new(Spike::new(&cc.egui_ctx, lines, &args)))
        }),
    )
}

/// Deterministic xorshift, so runs are comparable.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn synthetic_lines(count: usize) -> Vec<String> {
    const WORDS: &[&str] = &[
        "the",
        "editor",
        "rope",
        "buffer",
        "markdown",
        "render",
        "glyph",
        "atlas",
        "layout",
        "scroll",
        "a",
        "of",
        "and",
        "to",
        "in",
        "is",
        "block",
        "source",
        "patch",
        "epoch",
        "minimal",
        "live",
        "pane",
        "**bold**",
        "_emphasis_",
        "`code`",
        "[link](https://example.com)",
        "fast",
        "local",
    ];
    const EXTRAS: &[&str] = &[
        "naïve café — “quotes” → ∑ λ ≠ ∞",
        "日本語のテキストと中文混排",
        "emoji 🦀 🚀 ✨ in prose",
        "Ελληνικά и кириллица",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    (0..count)
        .map(|i| {
            let words = match rng.below(20) {
                0..=2 => 0,
                3 => 60 + rng.below(80),
                _ => 3 + rng.below(25),
            };
            let mut line = match rng.below(40) {
                0 => "# ".to_owned(),
                1 => "- ".to_owned(),
                2 => "> ".to_owned(),
                _ => String::new(),
            };
            for w in 0..words {
                if w > 0 {
                    line.push(' ');
                }
                line.push_str(WORDS[rng.below(WORDS.len())]);
            }
            if i % 37 == 0 {
                line.push(' ');
                line.push_str(EXTRAS[(i / 37) % EXTRAS.len()]);
            }
            line
        })
        .collect()
}

#[derive(Default)]
struct Samples(VecDeque<f32>);

impl Samples {
    fn push(&mut self, ms: f32) {
        if self.0.len() == STATS_WINDOW {
            self.0.pop_front();
        }
        self.0.push_back(ms);
    }

    fn summary(&self) -> Summary {
        summarize(self.0.iter().copied().collect())
    }
}

struct Summary {
    p50: f32,
    p95: f32,
    max: f32,
}

fn summarize(mut v: Vec<f32>) -> Summary {
    if v.is_empty() {
        return Summary {
            p50: 0.0,
            p95: 0.0,
            max: 0.0,
        };
    }
    v.sort_by(f32::total_cmp);
    let at = |q: f32| v[((v.len() - 1) as f32 * q).round() as usize];
    Summary {
        p50: at(0.5),
        p95: at(0.95),
        max: at(1.0),
    }
}

struct Bench {
    until: Instant,
    started: Instant,
    cpu: Vec<f32>,
    lines_drawn: usize,
    interval: Vec<f32>,
}

struct Spike {
    lines: Vec<String>,
    char_counts: Vec<usize>,
    text: TextRenderer,
    heights: HeightCache,
    anchor: ScrollAnchor,
    config: TextConfig,
    autoscroll: bool,
    speed: f32,
    jump: bool,
    rng: Rng,
    cpu: Samples,
    interval: Samples,
    last_frame: Option<Instant>,
    bench: Option<Bench>,
    bench_secs: Option<f32>,
    startup: Instant,
    first_frame_ms: Option<f32>,
}

impl Spike {
    fn new(ctx: &egui::Context, lines: Vec<String>, args: &Args) -> Self {
        let char_counts = lines.iter().map(|l| l.chars().count()).collect();
        Self {
            lines,
            char_counts,
            text: TextRenderer::new(ctx),
            heights: HeightCache::new([]),
            anchor: ScrollAnchor {
                line: args.goto,
                offset: 0.0,
            },
            config: TextConfig {
                monospace: args.monospace,
                font_size: 14.0,
                line_height: 20.0,
                wrap_width: args.wrap.then_some(0.0),
            },
            autoscroll: args.bench.is_some() && !args.jump,
            speed: args.speed,
            jump: args.jump,
            rng: Rng(42),
            cpu: Samples::default(),
            interval: Samples::default(),
            last_frame: None,
            bench: None,
            bench_secs: args.bench,
            startup: Instant::now(),
            first_frame_ms: None,
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.autoscroll, "autoscroll");
            ui.add(
                egui::Slider::new(&mut self.speed, 100.0..=20_000.0)
                    .logarithmic(true)
                    .suffix(" pt/s"),
            );
            ui.checkbox(&mut self.jump, "random jump");
            ui.checkbox(&mut self.config.monospace, "monospace");
            let mut wrap = self.config.wrap_width.is_some();
            if ui.checkbox(&mut wrap, "wrap").changed() {
                self.config.wrap_width = wrap.then_some(0.0);
            }
            ui.add(egui::Slider::new(&mut self.config.font_size, 8.0..=32.0).text("font"));
            self.config.line_height = (self.config.font_size * 1.45).round();
        });
        let (cpu, interval) = (self.cpu.summary(), self.interval.summary());
        let atlas = self.text.atlas_stats();
        ui.label(format!(
            "lines {}  measured {}  cached {}  top {}  |  layout+mesh ms p50 {:.2} p95 {:.2} max {:.2}  |  frame interval ms p50 {:.2} p95 {:.2} max {:.2}  |  atlas pages {} glyphs {} new {} resets {}  |  first frame {:.0} ms",
            self.lines.len(),
            self.heights.measured_count(),
            self.text.cached_lines(),
            self.anchor.line,
            cpu.p50, cpu.p95, cpu.max,
            interval.p50, interval.p95, interval.max,
            atlas.pages, atlas.glyphs, atlas.rasterized_last_frame, atlas.resets,
            self.first_frame_ms.unwrap_or(0.0),
        ));
    }

    fn text_area(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        let response = ui.allocate_rect(rect, Sense::click_and_drag());
        let text_rect =
            Rect::from_min_max(rect.min, pos2(rect.max.x - SCROLLBAR_WIDTH, rect.max.y));
        let bar_rect = Rect::from_min_max(pos2(text_rect.max.x, rect.min.y), rect.max);
        let viewport = text_rect.height();

        if let Some(wrap) = &mut self.config.wrap_width {
            *wrap = text_rect.width() - 2.0 * PADDING;
        }
        let ppp = ui.ctx().pixels_per_point();
        let started = Instant::now();
        if self.text.begin_frame(self.config, ppp) {
            let text = &self.text;
            self.heights
                .reset_estimates(self.char_counts.iter().map(|&c| text.estimate_height(c)));
        }

        // Input → scroll.
        let (dt, wheel, page_down, page_up, home, end) = ui.input(|i| {
            (
                i.stable_dt,
                i.smooth_scroll_delta.y,
                i.key_pressed(Key::PageDown),
                i.key_pressed(Key::PageUp),
                i.modifiers.ctrl && i.key_pressed(Key::Home),
                i.modifiers.ctrl && i.key_pressed(Key::End),
            )
        });
        let mut delta = 0.0;
        if response.hovered() {
            delta -= wheel;
        }
        if page_down {
            delta += viewport;
        }
        if page_up {
            delta -= viewport;
        }
        if self.autoscroll {
            delta += self.speed * dt;
        }
        if home {
            self.anchor = ScrollAnchor::default();
        }
        if end {
            delta = f32::INFINITY;
        }
        if self.jump {
            self.anchor = ScrollAnchor {
                line: self.rng.below(self.lines.len().max(1)),
                offset: 0.0,
            };
        }
        if response.dragged()
            && response
                .interact_pointer_pos()
                .is_some_and(|p| bar_rect.contains(p) || p.x > text_rect.max.x)
        {
            let y = response.interact_pointer_pos().unwrap().y;
            let frac = ((y - bar_rect.min.y) / bar_rect.height()).clamp(0.0, 1.0);
            self.anchor = self.heights.line_at(f64::from(frac) * self.heights.total());
        }
        self.anchor = self.heights.scroll_by(self.anchor, delta, viewport);
        if self.autoscroll
            && self.heights.anchor_y(self.anchor) + f64::from(viewport)
                >= self.heights.total() - 1.0
        {
            self.anchor = ScrollAnchor::default();
        }

        // Paint visible lines, measuring as we go.
        let mut meshes = GlyphMeshes::default();
        let mut y = text_rect.min.y - self.anchor.offset;
        let mut line = self.anchor.line;
        while y < text_rect.max.y && line < self.lines.len() {
            let height = self.text.line_height(line, &self.lines[line]);
            self.heights.set_measured(line, height);
            if line == self.anchor.line && self.anchor.offset > height {
                self.anchor.offset = height;
            }
            self.text.draw_line(
                &mut meshes,
                line,
                &self.lines[line],
                pos2(text_rect.min.x + PADDING, y),
                Color32::from_gray(210),
            );
            y += height;
            line += 1;
        }
        let painter = ui.painter_at(text_rect);
        self.text.end_frame(meshes, &painter);
        let cpu_ms = started.elapsed().as_secs_f32() * 1000.0;

        // Scrollbar.
        let total = self.heights.total().max(1.0) as f32;
        let thumb_h = (viewport / total * bar_rect.height())
            .max(20.0)
            .min(bar_rect.height());
        let thumb_y = self.heights.anchor_y(self.anchor) as f32 / total * bar_rect.height();
        let thumb = Rect::from_min_size(
            pos2(bar_rect.min.x + 2.0, bar_rect.min.y + thumb_y),
            vec2(SCROLLBAR_WIDTH - 4.0, thumb_h),
        );
        ui.painter()
            .rect_filled(bar_rect, 0.0, Color32::from_gray(24));
        ui.painter().rect_filled(thumb, 4.0, Color32::from_gray(90));

        self.record(ui.ctx(), cpu_ms, line - self.anchor.line);
    }

    fn record(&mut self, ctx: &egui::Context, cpu_ms: f32, lines_drawn: usize) {
        let now = Instant::now();
        let interval_ms = self.last_frame.map(|t| (now - t).as_secs_f32() * 1000.0);
        self.last_frame = Some(now);
        self.cpu.push(cpu_ms);
        if let Some(ms) = interval_ms {
            self.interval.push(ms);
        }
        if self.first_frame_ms.is_none() {
            self.first_frame_ms = Some(self.startup.elapsed().as_secs_f32() * 1000.0);
        }

        if self.bench.is_none()
            && let Some(secs) = self.bench_secs.take()
        {
            // Start after the first frame so font loading isn't counted.
            self.bench = Some(Bench {
                until: now + Duration::from_secs_f32(secs),
                started: now,
                cpu: Vec::new(),
                lines_drawn: 0,
                interval: Vec::new(),
            });
        } else if let Some(bench) = &mut self.bench {
            bench.cpu.push(cpu_ms);
            bench.lines_drawn += lines_drawn;
            bench.interval.extend(interval_ms);
            if now >= bench.until {
                self.finish_bench(ctx);
            }
        }
        if self.autoscroll || self.jump || self.bench.is_some() {
            ctx.request_repaint();
        }
    }

    fn finish_bench(&mut self, ctx: &egui::Context) {
        let bench = self.bench.take().expect("bench running");
        let secs = bench.started.elapsed().as_secs_f32();
        let frames = bench.cpu.len();
        let lines_per_frame = bench.lines_drawn as f32 / frames.max(1) as f32;
        let cpu_total: f32 = bench.cpu.iter().sum();
        let (cpu, interval) = (summarize(bench.cpu), summarize(bench.interval));
        let atlas = self.text.atlas_stats();
        println!(
            "mode={} lines={} frames={frames} fps={:.1}",
            if self.jump { "jump" } else { "autoscroll" },
            self.lines.len(),
            frames as f32 / secs,
        );
        println!(
            "layout+mesh ms   p50={:.2} p95={:.2} max={:.2}  lines/frame={lines_per_frame:.1}  us/line={:.1}",
            cpu.p50,
            cpu.p95,
            cpu.max,
            cpu_total * 1000.0 / bench.lines_drawn.max(1) as f32,
        );
        println!(
            "frame interval ms p50={:.2} p95={:.2} max={:.2}",
            interval.p50, interval.p95, interval.max
        );
        println!(
            "atlas pages={} glyphs={} resets={}  measured lines={}  first frame ms={:.0}",
            atlas.pages,
            atlas.glyphs,
            atlas.resets,
            self.heights.measured_count(),
            self.first_frame_ms.unwrap_or(0.0),
        );
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for Spike {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("controls").show(ui, |ui| self.controls(ui));
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_gray(16)))
            .show(ui, |ui| self.text_area(ui));
    }
}
