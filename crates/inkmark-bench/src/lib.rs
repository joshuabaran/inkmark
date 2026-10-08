//! Benchmark support for inkmark: deterministic fixtures, latency
//! summaries (p50/p95/max) written as JSON lines, an allocation counter,
//! and the report and compare steps `scripts/bench.sh` runs.
//!
//! Benches call [`Reporter::record`] with raw samples. When
//! `INKMARK_BENCH_OUT` names a file, each result is appended to it as one
//! JSON object per line; `inkmark-bench report` merges those with
//! criterion's estimates into `target/bench/<sha>.{json,md}`.

pub mod fixtures;
pub mod report;

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether `INKMARK_BENCH_MODE=quick`: fewer iterations, for CI smoke runs.
pub fn quick() -> bool {
    std::env::var("INKMARK_BENCH_MODE").is_ok_and(|m| m == "quick")
}

/// `full` iterations normally, `quick` under [`quick`].
pub fn iterations(full: usize, quick_n: usize) -> usize {
    if quick() { quick_n } else { full }
}

/// p50, p95 and max (plus mean) of a set of samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
    pub mean: f64,
}

impl Summary {
    /// Nearest-rank quantiles over the sorted samples. Empty input is all
    /// zeros.
    pub fn of(samples: &[f64]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut v = samples.to_vec();
        v.sort_by(f64::total_cmp);
        let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
        Self {
            n: v.len(),
            p50: at(0.5),
            p95: at(0.95),
            max: v[v.len() - 1],
            mean: v.iter().sum::<f64>() / v.len() as f64,
        }
    }
}

/// Collects results for one suite and writes them out.
pub struct Reporter {
    suite: &'static str,
    filter: Option<String>,
}

impl Reporter {
    /// `suite` groups results in the report (e.g. "frames", "app").
    /// A first command-line argument that isn't a flag filters bench names
    /// by substring, as `cargo bench -- <filter>` does elsewhere.
    pub fn new(suite: &'static str) -> Self {
        let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
        Self { suite, filter }
    }

    /// Whether `name` should run under the command-line filter.
    pub fn wants(&self, name: &str) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|f| name.contains(f.as_str()))
    }

    /// Prints a summary of `samples_ms` and appends it to
    /// `$INKMARK_BENCH_OUT`. `budget_p95` is a hard limit the compare step
    /// enforces (PLAN.md's 16 ms). `extra` carries named numbers such as
    /// allocations per frame.
    pub fn record(
        &self,
        name: &str,
        samples_ms: &[f64],
        budget_p95: Option<f64>,
        extra: &[(&str, f64)],
    ) -> Summary {
        let s = Summary::of(samples_ms);
        let mut line = format!(
            "{}/{name}: n={} p50={:.3} p95={:.3} max={:.3} ms",
            self.suite, s.n, s.p50, s.p95, s.max
        );
        for (k, v) in extra {
            line.push_str(&format!(" {k}={v:.1}"));
        }
        if let Some(b) = budget_p95
            && s.p95 > b
        {
            line.push_str(&format!("  OVER BUDGET ({b} ms)"));
        }
        println!("{line}");
        let extra: serde_json::Map<String, serde_json::Value> = extra
            .iter()
            .map(|(k, v)| ((*k).to_owned(), serde_json::json!(v)))
            .collect();
        let json = serde_json::json!({
            "suite": self.suite,
            "name": name,
            "unit": "ms",
            "n": s.n,
            "p50": s.p50,
            "p95": s.p95,
            "max": s.max,
            "mean": s.mean,
            "budget_p95": budget_p95,
            "extra": extra,
        });
        if let Some(path) = std::env::var_os("INKMARK_BENCH_OUT")
            && let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = writeln!(f, "{json}");
        }
        s
    }
}

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

/// A global allocator that counts allocations, for "allocations per
/// frame" numbers. Install it in a bench binary:
/// `#[global_allocator] static A: CountingAlloc = CountingAlloc;`
pub struct CountingAlloc;

// SAFETY: every call is forwarded to the system allocator unchanged; the
// counters are side effects only.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Allocations and bytes allocated so far (all threads), when
/// [`CountingAlloc`] is the global allocator; zeros otherwise.
pub fn allocations() -> (u64, u64) {
    (
        ALLOCS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_picks_nearest_ranks() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = Summary::of(&samples);
        assert_eq!(s.n, 100);
        assert_eq!(s.p50, 51.0);
        assert_eq!(s.p95, 95.0);
        assert_eq!(s.max, 100.0);
        assert_eq!(Summary::of(&[]), Summary::default());
    }
}
