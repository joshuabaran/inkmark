//! A session log for unexpected exits.
//!
//! The Apps menu launches inkmark in a scope whose stderr is `/dev/null`,
//! so a panic never reaches the journal and a clean-looking quit leaves no
//! trace. This file records why the process ended: a close request, the
//! event loop returning, or a panic. When stderr is discarded, it is also
//! pointed at the same file, so a Wayland library message is kept. A
//! terminal, a redirect, and a pipe are left alone.
//!
//! Past 1 MiB the file is renamed with a `.1` suffix and a new one starts.
//! One backup is kept, including while a window stays open.

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static SESSION_LOG: SessionLog = SessionLog;

static LOG: Mutex<Option<LogFile>> = Mutex::new(None);

/// Set once stderr has been pointed at the log, so a rotation follows it.
static STDERR_MIRRORED: AtomicBool = AtomicBool::new(false);

/// How big `session.log` may grow before the previous segment is kept as
/// `session.log.1`. A 10 s heartbeat is about a day per segment.
const LOG_CAP: u64 = 1024 * 1024;

struct LogFile {
    path: PathBuf,
    file: File,
}

/// Opens the session log. A discarded stderr is pointed at it too.
/// A missing home directory leaves logging off; the app still runs.
pub fn init() {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Some(file) = open_log(&path) else {
        return;
    };
    // A menu launch points stderr at /dev/null. Duplicate the log there
    // before the window exists, so a protocol error on the way out is in
    // the file. A redirect or a pipe still belongs to the caller. The e2e
    // harness captures stderr itself and leaves this alone, so Wayland
    // debug does not mix into the session lines.
    if std::env::var_os("INKMARK_E2E").is_none() && stderr_is_discarded() {
        mirror_stderr(&file);
    }
    if let Ok(mut guard) = LOG.lock() {
        *guard = Some(LogFile {
            path: path.clone(),
            file,
        });
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    install_loggers();
    line(&format!(
        "start pid={} version={} log={} args={args:?}",
        std::process::id(),
        env!("CARGO_PKG_VERSION"),
        path.display()
    ));
    install_panic_hook();
}

/// Opens `path` for tests. Does not install a panic hook or touch stderr.
#[cfg(test)]
pub fn init_at(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    if let Ok(mut guard) = LOG.lock() {
        *guard = Some(LogFile {
            path: path.to_path_buf(),
            file,
        });
    }
    line("start test");
}

pub fn line(message: &str) {
    let Ok(mut guard) = LOG.lock() else {
        return;
    };
    let Some(slot) = guard.as_mut() else {
        return;
    };
    rotate_if_over(&slot.path, &mut slot.file, LOG_CAP);
    let _ = writeln!(slot.file, "{} {message}", stamp());
    let _ = slot.file.flush();
}

/// Seconds between `alive` lines. `INKMARK_HEARTBEAT_SECS` overrides the default.
pub fn heartbeat_every() -> Duration {
    let secs = std::env::var("INKMARK_HEARTBEAT_SECS")
        .ok()
        .and_then(|text| text.parse().ok())
        .filter(|secs: &u64| *secs > 0)
        .unwrap_or(10);
    Duration::from_secs(secs)
}

/// `INKMARK_E2E_QUIT_AFTER` seconds: the e2e harness asks the window to close.
pub fn quit_after() -> Option<Duration> {
    std::env::var("INKMARK_E2E_QUIT_AFTER")
        .ok()
        .and_then(|text| text.parse().ok())
        .filter(|secs: &u64| *secs > 0)
        .map(Duration::from_secs)
}

fn log_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("INKMARK_LOG") {
        return Some(PathBuf::from(path));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    state.map(|dir| dir.join("inkmark/session.log"))
}

/// Warn and error records from `log` and `tracing` (winit's Wayland
/// dispatch failure is a tracing error). Debug stays off: Wayland protocol
/// traffic is enabled only by the e2e harness, on stderr.
fn install_loggers() {
    let _ = log::set_logger(&SESSION_LOG);
    log::set_max_level(log::LevelFilter::Warn);
    let _ = tracing::subscriber::set_global_default(SessionTrace);
}

fn open_log(path: &Path) -> Option<File> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    rotate_if_over(path, &mut file, LOG_CAP);
    Some(file)
}

/// stderr is `/dev/null`, or already closed. A terminal, a redirect, and a
/// pipe still have a reader.
fn stderr_is_discarded() -> bool {
    if std::io::stderr().is_terminal() {
        return false;
    }
    let Ok(err) = rustix::fs::fstat(rustix::stdio::stderr()) else {
        return true;
    };
    let Ok(null) = File::open("/dev/null") else {
        return false;
    };
    let Ok(null_stat) = rustix::fs::fstat(&null) else {
        return false;
    };
    same_file(&err, &null_stat)
}

fn same_file(a: &rustix::fs::Stat, b: &rustix::fs::Stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

fn mirror_stderr(file: &File) {
    if let Ok(copy) = file.try_clone()
        && rustix::stdio::dup2_stderr(copy).is_ok()
    {
        STDERR_MIRRORED.store(true, Ordering::Relaxed);
    }
}

/// Rename `path` to `path.1` once it is past `cap`, and keep writing to a
/// new file. Failure leaves the current file in place.
fn rotate_if_over(path: &Path, file: &mut File, cap: u64) {
    let Ok(len) = file.metadata().map(|meta| meta.len()) else {
        return;
    };
    if len <= cap {
        return;
    }
    let backup = backup_path(path);
    if fs::rename(path, &backup).is_err() {
        return;
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(fresh) => {
            // Point stderr at the new file before dropping the old
            // descriptor, which still refers to the renamed inode.
            if STDERR_MIRRORED.load(Ordering::Relaxed) {
                mirror_stderr(&fresh);
            }
            *file = fresh;
        }
        Err(_) => {
            let _ = fs::rename(&backup, path);
        }
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

struct SessionLog;

impl log::Log for SessionLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            line(&format!(
                "log {} {}: {}",
                record.level(),
                record.target(),
                record.args()
            ));
        }
    }

    fn flush(&self) {}
}

struct SessionTrace;

impl tracing::Subscriber for SessionTrace {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::WARN)
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut message = String::new();
        event.record(&mut FieldVisit(&mut message));
        line(&format!(
            "tracing {} {}: {message}",
            event.metadata().level(),
            event.metadata().target()
        ));
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

struct FieldVisit<'a>(&'a mut String);

impl tracing::field::Visit for FieldVisit<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(&format!("{}={value:?}", field.name()));
    }
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        line(&format!("panic {info}\n{backtrace}"));
        previous(info);
    }));
}

fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's civil-from-days, UTC.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_appended_with_a_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        init_at(&path);
        line("hello from the test");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("start test"), "{text}");
        assert!(text.contains("hello from the test"), "{text}");
        assert!(text.contains('T') && text.contains('Z'), "{text}");
    }

    #[test]
    fn unix_epoch_is_the_civil_date() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
    }

    #[test]
    fn a_full_log_is_renamed_and_the_next_segment_starts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "first").unwrap();
        rotate_if_over(&path, &mut file, 0);
        writeln!(file, "second").unwrap();
        file.flush().unwrap();

        let backup = fs::read_to_string(backup_path(&path)).unwrap();
        let current = fs::read_to_string(&path).unwrap();
        assert_eq!(backup, "first\n");
        assert_eq!(current, "second\n");
    }

    #[test]
    fn a_second_rotation_replaces_the_one_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "first").unwrap();
        rotate_if_over(&path, &mut file, 0);
        writeln!(file, "second").unwrap();
        rotate_if_over(&path, &mut file, 0);
        writeln!(file, "third").unwrap();
        file.flush().unwrap();

        let backup = fs::read_to_string(backup_path(&path)).unwrap();
        let current = fs::read_to_string(&path).unwrap();
        assert_eq!(backup, "second\n");
        assert_eq!(current, "third\n");
        assert!(!dir.path().join("session.log.2").exists());
    }

    #[test]
    fn a_short_log_stays_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "still here").unwrap();
        rotate_if_over(&path, &mut file, LOG_CAP);
        file.flush().unwrap();
        assert!(!backup_path(&path).exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "still here\n");
    }

    #[test]
    fn devnull_matches_itself_and_not_a_redirect() {
        let null_a = File::open("/dev/null").unwrap();
        let null_b = File::open("/dev/null").unwrap();
        let a = rustix::fs::fstat(&null_a).unwrap();
        let b = rustix::fs::fstat(&null_b).unwrap();
        assert!(same_file(&a, &b));

        let dir = tempfile::tempdir().unwrap();
        let redirected = File::create(dir.path().join("err.txt")).unwrap();
        let redirected = rustix::fs::fstat(&redirected).unwrap();
        assert!(!same_file(&a, &redirected));
    }

    #[test]
    fn a_piped_stderr_is_not_discarded() {
        // `cargo test` captures stderr on a pipe. A terminal is not discarded
        // either. Only `/dev/null` (a menu launch) and a closed fd are.
        assert!(
            !stderr_is_discarded(),
            "this test's stderr should be a pipe or a terminal"
        );
    }
}
