//! `INKMARK_MEASURE=1 inkmark [file]`: prints the numbers from PLAN.md's
//! "What to measure" for this run, then quits. `scripts/measure.sh` drives
//! it, and `scripts/bench.sh full` runs that.
//!
//! The phases, in order:
//! 1. startup to first frame, and to the full parse settling (with RSS);
//! 2. five seconds of split-mode scrolling with both minimaps;
//! 3. typing: synthetic key events injected into the real window (code
//!    pane, then live pane), one every 100 ms, so debounced parses land
//!    between keystrokes as they do for a person;
//! 4. saving, when the file is a scratch copy (`INKMARK_MEASURE_SAVE=1`);
//! 5. two seconds to settle, then five seconds idle: CPU time and how many
//!    frames the app asked for.
//!
//! `INKMARK_MEASURE_LABEL` (e.g. `5mb`) is appended to each result's
//! name, and `INKMARK_BENCH_OUT` names a file to append JSON lines to, in
//! the format `inkmark-bench` reads. `/proc` reads make RSS and CPU time
//! Linux-only.

use std::time::{Duration, Instant};

use eframe::egui;

/// How long the scroll phase runs.
const SCROLL_FOR: Duration = Duration::from_secs(5);
/// Source lines scrolled per frame: a fast but readable flick.
const LINES_PER_FRAME: usize = 3;
/// Keystrokes typed into each pane.
const KEYSTROKES: usize = 40;
/// Time between keystrokes: a quick typist, slower than the parse debounce.
const KEY_INTERVAL: Duration = Duration::from_millis(100);
/// Saves timed in the save phase.
const SAVES: usize = 5;
/// How long the idle phase watches.
const IDLE_FOR: Duration = Duration::from_secs(5);
/// Quiet time before the idle phase starts counting, so the last save's
/// folder event and the last full parse aren't counted as idle wakes.
const IDLE_SETTLE: Duration = Duration::from_secs(2);

/// Which pane a step is about.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pane {
    Code,
    Live,
}

pub enum Step {
    Idle,
    /// Scroll both panes so this source line is at the top.
    ScrollTo(usize),
    /// Focus `pane` and put its caret in the middle of the document.
    Caret(Pane),
    /// Save the document; report how long it took with [`Measure::saved`].
    Save,
    Quit,
}

enum Phase {
    Startup,
    Scroll(Scroll),
    Typing(Typing),
    Save { done: usize },
    Idle(IdleWatch),
    Done,
}

struct Scroll {
    started: Instant,
    last: Instant,
    intervals_ms: Vec<f64>,
    line: usize,
}

struct Typing {
    pane: Pane,
    /// When the caret was placed; typing waits a moment after it.
    placed: Option<Instant>,
    last_key: Option<Instant>,
    /// When the pending keystroke went into the frame's input.
    injected: Option<Instant>,
    /// Keystroke → end of the app's frame (CPU).
    update_ms: Vec<f64>,
    /// Keystroke → the next frame's input (tessellation, GPU submit and
    /// the wait for the swap included).
    frame_ms: Vec<f64>,
    awaiting_next: Option<Instant>,
}

impl Typing {
    fn new(pane: Pane) -> Self {
        Self {
            pane,
            placed: None,
            last_key: None,
            injected: None,
            update_ms: Vec::new(),
            frame_ms: Vec::new(),
            awaiting_next: None,
        }
    }
}

struct IdleWatch {
    started: Instant,
    cpu_start: Option<f64>,
    frames: usize,
    /// Why egui was asked to draw each idle frame (file:line), counted.
    causes: std::collections::BTreeMap<String, usize>,
}

pub struct Measure {
    start: Instant,
    label: String,
    save: bool,
    first_frame_ms: Option<f64>,
    settled_ms: Option<f64>,
    rss_mb: Option<f64>,
    phase: Phase,
    /// Scroll frame intervals and the phase's length in seconds.
    scrolled: Option<(Vec<f64>, f64)>,
    typed: Vec<(Pane, Vec<f64>, Vec<f64>)>,
    save_ms: Vec<f64>,
}

impl Measure {
    /// Enabled by `INKMARK_MEASURE`; `start` is when `main` began.
    pub fn from_env(start: Instant) -> Option<Self> {
        std::env::var_os("INKMARK_MEASURE")?;
        Some(Self {
            start,
            label: std::env::var("INKMARK_MEASURE_LABEL").unwrap_or_default(),
            save: std::env::var_os("INKMARK_MEASURE_SAVE").is_some(),
            first_frame_ms: None,
            settled_ms: None,
            rss_mb: None,
            phase: Phase::Startup,
            scrolled: None,
            typed: Vec::new(),
            save_ms: Vec::new(),
        })
    }

    /// Call from `raw_input_hook`, before the frame runs: injects the next
    /// keystroke when one is due.
    pub fn input(&mut self, raw: &mut egui::RawInput) {
        let Phase::Typing(t) = &mut self.phase else {
            return;
        };
        let now = Instant::now();
        if let Some(pressed) = t.awaiting_next.take() {
            t.frame_ms.push(ms_between(pressed, now));
        }
        let Some(placed) = t.placed else { return };
        if now - placed < Duration::from_millis(300) {
            return;
        }
        if t.last_key.is_some_and(|k| now - k < KEY_INTERVAL) {
            return;
        }
        raw.events.push(egui::Event::Text("x".into()));
        t.last_key = Some(now);
        t.injected = Some(now);
    }

    /// Call at the end of every frame.
    pub fn frame(&mut self, ctx: &egui::Context, settled: bool, line_count: usize) -> Step {
        let now = Instant::now();
        let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;
        // Every phase but idle keeps frames coming.
        if !matches!(self.phase, Phase::Idle(_)) {
            ctx.request_repaint();
        }
        match &mut self.phase {
            Phase::Startup => {
                if self.first_frame_ms.is_none() {
                    self.first_frame_ms = Some(ms(self.start));
                } else if settled {
                    self.settled_ms = Some(ms(self.start));
                    self.rss_mb = rss_mb();
                    self.phase = Phase::Scroll(Scroll {
                        started: now,
                        last: now,
                        intervals_ms: Vec::new(),
                        line: 0,
                    });
                }
                Step::Idle
            }
            Phase::Scroll(scroll) => {
                if scroll.line > 0 {
                    scroll
                        .intervals_ms
                        .push((now - scroll.last).as_secs_f64() * 1000.0);
                }
                scroll.last = now;
                if now - scroll.started >= SCROLL_FOR || scroll.line + LINES_PER_FRAME >= line_count
                {
                    let secs = (scroll.last - scroll.started).as_secs_f64();
                    self.scrolled = Some((std::mem::take(&mut scroll.intervals_ms), secs));
                    self.phase = Phase::Typing(Typing::new(Pane::Code));
                    return Step::Idle;
                }
                scroll.line += LINES_PER_FRAME;
                Step::ScrollTo(scroll.line)
            }
            Phase::Typing(t) => {
                if t.placed.is_none() {
                    t.placed = Some(now);
                    return Step::Caret(t.pane);
                }
                if let Some(pressed) = t.injected.take() {
                    t.update_ms.push(ms_between(pressed, now));
                    t.awaiting_next = Some(pressed);
                }
                if t.update_ms.len() >= KEYSTROKES && t.awaiting_next.is_none() {
                    let pane = t.pane;
                    let update = std::mem::take(&mut t.update_ms);
                    let frames = std::mem::take(&mut t.frame_ms);
                    self.typed.push((pane, update, frames));
                    self.phase = match pane {
                        Pane::Code => Phase::Typing(Typing::new(Pane::Live)),
                        Pane::Live if self.save => Phase::Save { done: 0 },
                        Pane::Live => idle_phase(now),
                    };
                }
                Step::Idle
            }
            Phase::Save { done } => {
                if *done >= SAVES {
                    self.phase = idle_phase(now);
                    return Step::Idle;
                }
                *done += 1;
                Step::Save
            }
            Phase::Idle(idle) => {
                if idle.cpu_start.is_none() {
                    // Settling: the frames from the phases before still
                    // land here. Count from the first frame after it.
                    if now - idle.started >= IDLE_SETTLE {
                        idle.cpu_start = cpu_seconds();
                        idle.started = now;
                    }
                    return Step::Idle;
                }
                idle.frames += 1;
                for cause in ctx.repaint_causes() {
                    *idle.causes.entry(cause.to_string()).or_default() += 1;
                }
                if now - idle.started < IDLE_FOR {
                    return Step::Idle;
                }
                let secs = (now - idle.started).as_secs_f64();
                let cpu = match (idle.cpu_start, cpu_seconds()) {
                    (Some(a), Some(b)) => Some((b - a) / secs * 100.0),
                    _ => None,
                };
                let frames = idle.frames as f64 / secs;
                for (cause, n) in &idle.causes {
                    println!("idle repaint cause: {cause} ×{n}");
                }
                self.phase = Phase::Done;
                self.report(cpu, frames);
                Step::Quit
            }
            Phase::Done => Step::Quit,
        }
    }

    /// The save step took `took`.
    pub fn saved(&mut self, took: Duration) {
        self.save_ms.push(took.as_secs_f64() * 1000.0);
    }

    fn name(&self, base: &str) -> String {
        if self.label.is_empty() {
            base.to_owned()
        } else {
            format!("{base}_{}", self.label)
        }
    }

    fn report(&self, idle_cpu: Option<f64>, idle_fps: f64) {
        let f = |v: Option<f64>| v.map_or("-".to_owned(), |v| format!("{v:.0}"));
        println!("first frame ms: {}", f(self.first_frame_ms));
        println!("parse settled ms: {}", f(self.settled_ms));
        println!("rss after settle MB: {}", f(self.rss_mb));
        let one = |name: &str, v: Option<f64>| {
            if let Some(v) = v {
                emit(&self.name(name), &[v], None, &[]);
            }
        };
        one("startup_first_frame", self.first_frame_ms);
        one("startup_parse_settled", self.settled_ms);
        if let Some(rss) = self.rss_mb {
            emit(
                &self.name("rss_after_settle"),
                &[0.0],
                None,
                &[("rss_mb", rss)],
            );
        }
        if let Some((v, secs)) = &self.scrolled
            && !v.is_empty()
        {
            let (p50, p95, max) = summary(v);
            let fps = v.len() as f64 / secs;
            println!(
                "split scroll, both minimaps: {} frames, {fps:.0} fps, frame interval ms p50 {p50:.2} p95 {p95:.2} max {max:.2}",
                v.len()
            );
            emit(
                &self.name("split_scroll_frame_interval"),
                v,
                None,
                &[("fps", fps)],
            );
        }
        for (pane, update, frames) in &self.typed {
            let pane = match pane {
                Pane::Code => "code",
                Pane::Live => "live",
            };
            let (u, fr) = (summary(update), summary(frames));
            println!(
                "{pane} keystroke → frame end ms p50 {:.2} p95 {:.2} max {:.2}; → next frame p50 {:.2} p95 {:.2} max {:.2}",
                u.0, u.1, u.2, fr.0, fr.1, fr.2
            );
            emit(
                &self.name(&format!("{pane}_keystroke_to_frame_end")),
                update,
                Some(16.0),
                &[],
            );
            emit(
                &self.name(&format!("{pane}_keystroke_to_next_frame")),
                frames,
                None,
                &[],
            );
        }
        if !self.save_ms.is_empty() {
            let s = summary(&self.save_ms);
            println!("save ms p50 {:.2} max {:.2}", s.0, s.2);
            emit(&self.name("save"), &self.save_ms, None, &[]);
        }
        println!(
            "idle: {} % CPU, {idle_fps:.1} frames/s",
            idle_cpu.map_or("-".to_owned(), |c| format!("{c:.2}"))
        );
        emit(
            &self.name("idle"),
            &[0.0],
            None,
            &[
                ("cpu_percent", idle_cpu.unwrap_or(-1.0)),
                ("frames_per_s", idle_fps),
            ],
        );
    }
}

fn idle_phase(now: Instant) -> Phase {
    Phase::Idle(IdleWatch {
        started: now,
        cpu_start: None,
        frames: 0,
        causes: Default::default(),
    })
}

fn ms_between(a: Instant, b: Instant) -> f64 {
    b.saturating_duration_since(a).as_secs_f64() * 1000.0
}

/// p50, p95, max.
fn summary(v: &[f64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut v = v.to_vec();
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    (at(0.5), at(0.95), at(1.0))
}

/// Appends one result to `$INKMARK_BENCH_OUT` as `inkmark-bench` writes
/// them (suite "app").
fn emit(name: &str, samples: &[f64], budget: Option<f64>, extra: &[(&str, f64)]) {
    use std::io::Write;
    let Some(path) = std::env::var_os("INKMARK_BENCH_OUT") else {
        return;
    };
    let (p50, p95, max) = summary(samples);
    let mean = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
    let num = |v: f64| {
        if v.is_finite() {
            format!("{v}")
        } else {
            "null".into()
        }
    };
    let extra: Vec<String> = extra
        .iter()
        .map(|(k, v)| format!("\"{k}\":{}", num(*v)))
        .collect();
    let line = format!(
        "{{\"suite\":\"app\",\"name\":\"{name}\",\"unit\":\"ms\",\"n\":{},\"p50\":{},\"p95\":{},\"max\":{},\"mean\":{},\"budget_p95\":{},\"extra\":{{{}}}}}",
        samples.len(),
        num(p50),
        num(p95),
        num(max),
        num(mean),
        budget.map_or("null".into(), num),
        extra.join(",")
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
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

/// User plus system CPU time of this process, from /proc, in seconds.
fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the parenthesised command name; utime and stime are
    // the 14th and 15th fields overall, in clock ticks (100 per second
    // on Linux).
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0)
}
