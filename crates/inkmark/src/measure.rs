//! `INKMARK_MEASURE=1 inkmark [file]`: prints the numbers from PLAN.md's
//! "What to measure" for this run (startup, parse, memory, split-mode
//! scrolling with both minimaps), then quits. `scripts/measure.sh` drives it.

use std::time::{Duration, Instant};

use eframe::egui;

/// How long the scroll phase runs.
const SCROLL_FOR: Duration = Duration::from_secs(5);
/// Source lines scrolled per frame: a fast but readable flick.
const LINES_PER_FRAME: usize = 3;

pub enum Step {
    Idle,
    /// Scroll both panes so this source line is at the top.
    ScrollTo(usize),
    Quit,
}

pub struct Measure {
    start: Instant,
    first_frame_ms: Option<f64>,
    settled_ms: Option<f64>,
    rss_mb: Option<f64>,
    scroll: Option<Scroll>,
    done: bool,
}

struct Scroll {
    started: Instant,
    last: Instant,
    intervals_ms: Vec<f64>,
    line: usize,
}

impl Measure {
    /// Enabled by `INKMARK_MEASURE`; `start` is when `main` began.
    pub fn from_env(start: Instant) -> Option<Self> {
        std::env::var_os("INKMARK_MEASURE").map(|_| Self {
            start,
            first_frame_ms: None,
            settled_ms: None,
            rss_mb: None,
            scroll: None,
            done: false,
        })
    }

    /// Call at the end of every frame.
    pub fn frame(&mut self, ctx: &egui::Context, settled: bool, line_count: usize) -> Step {
        if self.done {
            return Step::Quit;
        }
        ctx.request_repaint();
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        if self.first_frame_ms.is_none() {
            self.first_frame_ms = Some(ms(self.start));
            return Step::Idle;
        }
        if self.settled_ms.is_none() {
            if settled {
                self.settled_ms = Some(ms(self.start));
                self.rss_mb = rss_mb();
            }
            return Step::Idle;
        }
        let now = Instant::now();
        let scroll = self.scroll.get_or_insert(Scroll {
            started: now,
            last: now,
            intervals_ms: Vec::new(),
            line: 0,
        });
        if scroll.line > 0 {
            scroll
                .intervals_ms
                .push((now - scroll.last).as_secs_f64() * 1000.0);
        }
        scroll.last = now;
        if now - scroll.started >= SCROLL_FOR || scroll.line + LINES_PER_FRAME >= line_count {
            self.report();
            self.done = true;
            return Step::Quit;
        }
        scroll.line += LINES_PER_FRAME;
        Step::ScrollTo(scroll.line)
    }

    fn report(&self) {
        let f = |v: Option<f64>| v.map_or("-".to_owned(), |v| format!("{v:.0}"));
        println!("first frame ms: {}", f(self.first_frame_ms));
        println!("parse settled ms: {}", f(self.settled_ms));
        println!("rss after settle MB: {}", f(self.rss_mb));
        let Some(scroll) = &self.scroll else { return };
        let mut v = scroll.intervals_ms.clone();
        if v.is_empty() {
            return;
        }
        v.sort_by(f64::total_cmp);
        let at = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        let secs = (scroll.last - scroll.started).as_secs_f64();
        println!(
            "split scroll, both minimaps: {} frames, {:.0} fps, frame interval ms p50 {:.2} p95 {:.2} max {:.2}",
            v.len(),
            v.len() as f64 / secs,
            at(0.5),
            at(0.95),
            at(1.0),
        );
    }
}

/// Resident memory from /proc, in MB.
fn rss_mb() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: f64 = status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kb / 1024.0)
}
