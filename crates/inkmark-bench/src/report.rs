//! Merges a run's results into one JSON report and a Markdown table, and
//! compares two reports.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Value, json};

/// One measured number set, whatever produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub suite: String,
    pub name: String,
    pub n: u64,
    pub p50: f64,
    pub p95: Option<f64>,
    pub max: Option<f64>,
    pub budget_p95: Option<f64>,
    pub extra: BTreeMap<String, f64>,
}

impl Row {
    pub fn key(&self) -> String {
        format!("{}/{}", self.suite, self.name)
    }

    fn to_json(&self) -> Value {
        json!({
            "suite": self.suite,
            "name": self.name,
            "unit": "ms",
            "n": self.n,
            "p50": self.p50,
            "p95": self.p95,
            "max": self.max,
            "budget_p95": self.budget_p95,
            "extra": self.extra,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            suite: v.get("suite")?.as_str()?.to_owned(),
            name: v.get("name")?.as_str()?.to_owned(),
            n: v.get("n").and_then(Value::as_u64).unwrap_or(0),
            p50: v.get("p50")?.as_f64()?,
            p95: v.get("p95").and_then(Value::as_f64),
            max: v.get("max").and_then(Value::as_f64),
            budget_p95: v.get("budget_p95").and_then(Value::as_f64),
            extra: v
                .get("extra")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_f64()?)))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// Rows from a JSON-lines file written by [`crate::Reporter`]. A later
/// line for the same bench replaces an earlier one.
pub fn read_lines(path: &Path) -> io::Result<Vec<Row>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).map_err(io::Error::other)?;
        if let Some(row) = Row::from_json(&v) {
            rows.insert(row.key(), row);
        }
    }
    Ok(rows.into_values().collect())
}

/// Criterion's latest estimate for every bench under `dir`
/// (`target/criterion`): the median, as suite "micro".
pub fn read_criterion(dir: &Path) -> io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    if !dir.exists() {
        return Ok(rows);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            if path.file_name().is_some_and(|n| n == "new") {
                if let Some(row) = criterion_row(&path) {
                    rows.push(row);
                }
            } else if path
                .file_name()
                .is_none_or(|n| n != "base" && n != "report")
            {
                stack.push(path);
            }
        }
    }
    rows.sort_by_key(Row::key);
    Ok(rows)
}

fn criterion_row(new: &Path) -> Option<Row> {
    let bench: Value =
        serde_json::from_str(&fs::read_to_string(new.join("benchmark.json")).ok()?).ok()?;
    let est: Value =
        serde_json::from_str(&fs::read_to_string(new.join("estimates.json")).ok()?).ok()?;
    let name = bench.get("full_id")?.as_str()?.to_owned();
    let ns = |k: &str| est.get(k)?.get("point_estimate")?.as_f64();
    let median = ns("median")?;
    let mut extra = BTreeMap::new();
    if let Some(mean) = ns("mean") {
        extra.insert("mean_ms".to_owned(), mean / 1e6);
    }
    if let Some(Value::Object(t)) = bench.get("throughput")
        && let Some(bytes) = t.get("Bytes").and_then(Value::as_f64)
    {
        // MB/s at the median.
        extra.insert("mb_per_s".to_owned(), bytes / (median / 1e9) / 1e6);
    }
    Some(Row {
        suite: "micro".to_owned(),
        name,
        n: 0,
        p50: median / 1e6,
        p95: None,
        max: None,
        budget_p95: None,
        extra,
    })
}

/// A whole run: where and when it ran, and every row.
pub struct Report {
    pub sha: String,
    pub mode: String,
    pub host: BTreeMap<String, String>,
    pub rows: Vec<Row>,
}

impl Report {
    pub fn to_json(&self) -> Value {
        json!({
            "sha": self.sha,
            "mode": self.mode,
            "host": self.host,
            "results": self.rows.iter().map(Row::to_json).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            sha: v.get("sha")?.as_str()?.to_owned(),
            mode: v
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            host: v
                .get("host")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                        .collect()
                })
                .unwrap_or_default(),
            rows: v
                .get("results")?
                .as_array()?
                .iter()
                .filter_map(Row::from_json)
                .collect(),
        })
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let v: Value =
            serde_json::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)?;
        Self::from_json(&v)
            .ok_or_else(|| io::Error::other(format!("{} is not a bench report", path.display())))
    }

    /// The report as Markdown: host details, then one table per suite.
    pub fn to_markdown(&self) -> String {
        let mut out = format!("# Bench results at `{}` ({})\n\n", self.sha, self.mode);
        out.push_str("| Host | |\n|---|---|\n");
        for (k, v) in &self.host {
            let _ = writeln!(out, "| {k} | {v} |");
        }
        let mut suites: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
        for r in &self.rows {
            suites.entry(&r.suite).or_default().push(r);
        }
        for (suite, rows) in suites {
            let _ = write!(out, "\n## {suite}\n\n");
            if suite == "micro" {
                out.push_str("| Bench | Median | Throughput |\n|---|---:|---:|\n");
                for r in rows {
                    let tput = r
                        .extra
                        .get("mb_per_s")
                        .map_or(String::new(), |v| format!("{v:.0} MB/s"));
                    let _ = writeln!(out, "| `{}` | {} | {tput} |", r.name, fmt_ms(r.p50));
                }
            } else {
                out.push_str("| Bench | n | p50 | p95 | max | Budget p95 | Notes |\n|---|---:|---:|---:|---:|---:|---|\n");
                for r in rows {
                    let opt = |v: Option<f64>| v.map_or("–".to_owned(), fmt_ms);
                    let notes: Vec<String> = r
                        .extra
                        .iter()
                        .map(|(k, v)| format!("{k} {}", fmt_num(*v)))
                        .collect();
                    let _ = writeln!(
                        out,
                        "| `{}` | {} | {} | {} | {} | {} | {} |",
                        r.name,
                        r.n,
                        fmt_ms(r.p50),
                        opt(r.p95),
                        opt(r.max),
                        opt(r.budget_p95),
                        notes.join(", ")
                    );
                }
            }
        }
        out
    }
}

/// Milliseconds with a unit that keeps three significant figures.
pub fn fmt_ms(ms: f64) -> String {
    if ms >= 100.0 {
        format!("{ms:.0} ms")
    } else if ms >= 1.0 {
        format!("{ms:.2} ms")
    } else if ms >= 0.001 {
        format!("{:.1} µs", ms * 1e3)
    } else {
        format!("{:.0} ns", ms * 1e6)
    }
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 || v.abs() >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

/// What `compare` found for one bench present in both reports.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Ok,
    /// The median got slower by more than the threshold.
    Slower,
    /// p95 is over the bench's hard budget.
    OverBudget,
}

/// Compares `new` against `base`: a median more than `threshold` (0.15 =
/// 15%) slower, beyond a small noise floor, is a regression, and so is a
/// p95 over the bench's budget. Returns the Markdown table and how many
/// benches failed.
pub fn compare(base: &Report, new: &Report, threshold: f64) -> (String, usize) {
    /// Differences below this are timer noise, whatever the ratio.
    const FLOOR_MS: f64 = 0.0002;
    let base_rows: BTreeMap<String, &Row> = base.rows.iter().map(|r| (r.key(), r)).collect();
    let mut out = format!(
        "Comparing `{}` (new) with `{}` (base); threshold {:.0}% on medians.\n\n| Bench | Base p50 | New p50 | Change | New p95 | Verdict |\n|---|---:|---:|---:|---:|---|\n",
        new.sha,
        base.sha,
        threshold * 100.0
    );
    let mut failed = 0;
    for r in &new.rows {
        let over_budget = matches!((r.p95, r.budget_p95), (Some(p), Some(b)) if p > b);
        let Some(b) = base_rows.get(&r.key()) else {
            let verdict = if over_budget { "over budget" } else { "new" };
            failed += usize::from(over_budget);
            let _ = writeln!(
                out,
                "| `{}` | – | {} | – | {} | {verdict} |",
                r.key(),
                fmt_ms(r.p50),
                r.p95.map_or("–".into(), fmt_ms)
            );
            continue;
        };
        let change = if b.p50 > 0.0 {
            r.p50 / b.p50 - 1.0
        } else {
            0.0
        };
        let verdict = if over_budget {
            Verdict::OverBudget
        } else if change > threshold && r.p50 - b.p50 > FLOOR_MS {
            Verdict::Slower
        } else {
            Verdict::Ok
        };
        if verdict != Verdict::Ok {
            failed += 1;
        }
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {:+.1}% | {} | {} |",
            r.key(),
            fmt_ms(b.p50),
            fmt_ms(r.p50),
            change * 100.0,
            r.p95.map_or("–".into(), fmt_ms),
            match verdict {
                Verdict::Ok => "ok",
                Verdict::Slower => "**slower**",
                Verdict::OverBudget => "**over budget**",
            }
        );
    }
    (out, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, p50: f64, p95: Option<f64>, budget: Option<f64>) -> Row {
        Row {
            suite: "frames".into(),
            name: name.into(),
            n: 10,
            p50,
            p95,
            max: p95,
            budget_p95: budget,
            extra: BTreeMap::new(),
        }
    }

    fn report(sha: &str, rows: Vec<Row>) -> Report {
        Report {
            sha: sha.into(),
            mode: "quick".into(),
            host: BTreeMap::new(),
            rows,
        }
    }

    #[test]
    fn compare_flags_slower_medians_and_budget_breaches() {
        let base = report(
            "a",
            vec![
                row("steady", 1.0, Some(2.0), Some(16.0)),
                row("slower", 1.0, Some(2.0), None),
                row("budget", 1.0, Some(2.0), Some(16.0)),
                row("noise", 0.0001, None, None),
            ],
        );
        let new = report(
            "b",
            vec![
                row("steady", 1.1, Some(2.0), Some(16.0)),
                row("slower", 1.5, Some(2.0), None),
                row("budget", 1.0, Some(17.0), Some(16.0)),
                row("noise", 0.0002, None, None),
            ],
        );
        let (table, failed) = compare(&base, &new, 0.15);
        assert_eq!(failed, 2, "{table}");
        assert!(
            table.contains(
                "| `frames/slower` | 1.00 ms | 1.50 ms | +50.0% | 2.00 ms | **slower** |"
            ),
            "{table}"
        );
        assert!(table.contains("**over budget**"));
    }

    #[test]
    fn reports_round_trip_through_json() {
        let r = report("abc", vec![row("x", 1.0, Some(2.0), Some(16.0))]);
        let back = Report::from_json(&r.to_json()).unwrap();
        assert_eq!(back.rows, r.rows);
        assert!(
            r.to_markdown()
                .contains("| `x` | 10 | 1.00 ms | 2.00 ms | 2.00 ms | 16.00 ms |")
        );
    }
}
