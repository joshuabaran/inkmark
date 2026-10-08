//! `inkmark-bench`: the steps `scripts/bench.sh` runs.
//!
//! ```text
//! inkmark-bench fixtures [--dir DIR] [--notes N] [--images N]
//! inkmark-bench report --sha SHA --mode MODE --lines FILE --criterion DIR --out DIR [--host KEY=VALUE]...
//! inkmark-bench compare BASE.json NEW.json [--threshold 15] [--warn-only]
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use inkmark_bench::fixtures;
use inkmark_bench::report::{self, Report};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("fixtures") => cmd_fixtures(&args[1..]),
        Some("report") => cmd_report(&args[1..]),
        Some("compare") => cmd_compare(&args[1..]),
        _ => Err("usage: inkmark-bench fixtures|report|compare … (see the crate docs)".into()),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("inkmark-bench: {e}");
            ExitCode::from(2)
        }
    }
}

/// `--name value` pairs and positional arguments.
struct Args {
    flags: Vec<(String, String)>,
    switches: Vec<String>,
    positional: Vec<String>,
}

impl Args {
    fn parse(args: &[String], switches: &[&str]) -> Self {
        let mut out = Self {
            flags: Vec::new(),
            switches: Vec::new(),
            positional: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if let Some(name) = a.strip_prefix("--") {
                if switches.contains(&name) {
                    out.switches.push(name.to_owned());
                } else if let Some(v) = it.next() {
                    out.flags.push((name.to_owned(), v.clone()));
                }
            } else {
                out.positional.push(a.clone());
            }
        }
        out
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn all(&self, name: &str) -> impl Iterator<Item = &str> {
        self.flags
            .iter()
            .filter(move |(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn need(&self, name: &str) -> Result<&str, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn number(&self, name: &str, default: usize) -> Result<usize, String> {
        self.get(name).map_or(Ok(default), |v| {
            v.parse()
                .map_err(|_| format!("--{name}: not a number: {v}"))
        })
    }
}

fn cmd_fixtures(args: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(args, &[]);
    let dir = a.get("dir").map_or_else(fixtures::dir, PathBuf::from);
    let notes = a.number("notes", 10_000)?;
    let images = a.number("images", 24)?;
    let started = std::time::Instant::now();
    let manifest = fixtures::generate(&dir, notes, images).map_err(|e| e.to_string())?;
    eprintln!(
        "fixtures in {} ({:.1} s)",
        dir.display(),
        started.elapsed().as_secs_f64()
    );
    print!("{manifest}");
    Ok(ExitCode::SUCCESS)
}

/// CPU, core count and kernel, read from /proc (Linux only).
fn host_details() -> BTreeMap<String, String> {
    let mut host = BTreeMap::new();
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(model) = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
        {
            host.insert("cpu".into(), model.trim().to_owned());
        }
        let threads = cpuinfo
            .lines()
            .filter(|l| l.starts_with("processor"))
            .count();
        host.insert("threads".into(), threads.to_string());
    }
    if let Ok(kernel) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        host.insert("kernel".into(), kernel.trim().to_owned());
    }
    host
}

fn cmd_report(args: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(args, &[]);
    let sha = a.need("sha")?.to_owned();
    let mode = a.get("mode").unwrap_or("full").to_owned();
    let out = PathBuf::from(a.need("out")?);
    let mut rows = Vec::new();
    if let Some(lines) = a.get("lines") {
        rows.extend(report::read_lines(&PathBuf::from(lines)).map_err(|e| e.to_string())?);
    }
    if let Some(dir) = a.get("criterion") {
        rows.extend(report::read_criterion(&PathBuf::from(dir)).map_err(|e| e.to_string())?);
    }
    let mut host = host_details();
    for kv in a.all("host") {
        if let Some((k, v)) = kv.split_once('=')
            && !v.trim().is_empty()
        {
            host.insert(k.to_owned(), v.trim().to_owned());
        }
    }
    let report = Report {
        sha: sha.clone(),
        mode,
        host,
        rows,
    };
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let json = out.join(format!("{sha}.json"));
    let md = out.join(format!("{sha}.md"));
    let text = serde_json::to_string_pretty(&report.to_json()).map_err(|e| e.to_string())?;
    std::fs::write(&json, text + "\n").map_err(|e| e.to_string())?;
    std::fs::write(&md, report.to_markdown()).map_err(|e| e.to_string())?;
    println!("{}", json.display());
    println!("{}", md.display());
    Ok(ExitCode::SUCCESS)
}

fn cmd_compare(args: &[String]) -> Result<ExitCode, String> {
    let a = Args::parse(args, &["warn-only"]);
    let [base, new] = a.positional.as_slice() else {
        return Err("compare needs BASE.json and NEW.json".into());
    };
    let threshold: f64 = a
        .get("threshold")
        .map_or(Ok(15.0), str::parse)
        .map_err(|_| "--threshold: not a number")?;
    let base = Report::load(&PathBuf::from(base)).map_err(|e| e.to_string())?;
    let new = Report::load(&PathBuf::from(new)).map_err(|e| e.to_string())?;
    let (table, failed) = report::compare(&base, &new, threshold / 100.0);
    print!("{table}");
    println!(
        "\n{failed} regression(s) at {threshold:.0}% (base host: {}, new host: {})",
        base.host.get("cpu").map_or("?", String::as_str),
        new.host.get("cpu").map_or("?", String::as_str)
    );
    if failed > 0 && !a.switches.iter().any(|s| s == "warn-only") {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
